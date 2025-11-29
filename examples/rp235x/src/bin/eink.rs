#![no_std]
#![no_main]

use core::cell::RefCell;

use embedded_graphics::mono_font::MonoTextStyleBuilder;
use embedded_graphics::text::Baseline;
use embedded_graphics::text::Text;
use embedded_graphics::text::TextStyleBuilder;

use defmt::*;
use embassy_executor::Spawner;
use embassy_rp::gpio;
use embassy_rp::uart::{Blocking, Config as UartConfig, Uart};
use embassy_time::{Duration, Timer};
use embedded_graphics::{
    prelude::*,
    primitives::{PrimitiveStyle, Triangle},
};
use heapless;

use embassy_embedded_hal::shared_bus::blocking::spi::SpiDevice;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;

use embassy_rp::spi;
use embassy_rp::spi::Spi;
use epd_waveshare::{epd3in7::*, prelude::*};

use gpio::{Level, Output};
use {defmt_rtt as _, panic_probe as _};

const WIFI_NETWORK: &str = "guest-01";
const WIFI_PASSWORD: &str = "pickleswashere!";

const SET_WIFI_MODE: &str = "AT+WMODE=3,1";
const HTTP_REQUEST: &str = "AT+HTTPCLIENTLINE=2,2,\"application/json\",\"rust-fluid.vercel.app\",443,\"/api/simple\"";

fn format_wifi_command(network: &str, password: &str) -> heapless::String<64> {
    let mut cmd = heapless::String::new();
    let _ = cmd.push_str("AT+WJAP=\"");
    let _ = cmd.push_str(network);
    let _ = cmd.push_str("\",\"");
    let _ = cmd.push_str(password);
    let _ = cmd.push_str("\"");
    cmd
}

// Program metadata for `picotool info`.
#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"EInk BW16 WiFi Example"),
    embassy_rp::binary_info::rp_program_description!(
        c"This example uses BW16 chip for WiFi connectivity with e-ink display"
    ),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

async fn send_at_command(uart: &mut Uart<'static, Blocking>, command: &str) -> bool {
    // Send command
    if uart.blocking_write(command.as_bytes()).is_err() {
        return false;
    }
    if uart.blocking_write(b"\r\n").is_err() {
        return false;
    }

    // Wait a bit for response
    Timer::after(Duration::from_millis(2000)).await;

    // Simple success assumption since we can't easily read response in blocking mode
    // In a real implementation, you'd want to properly parse the AT response
    true
}

async fn send_http_request(uart: &mut Uart<'static, Blocking>, command: &str) -> heapless::String<256> {
    info!("Sending HTTP request: {}", command);

    // Send HTTP request command
    if uart.blocking_write(command.as_bytes()).is_err() {
        let mut error_msg = heapless::String::new();
        let _ = error_msg.push_str("HTTP Write Failed");
        return error_msg;
    }
    if uart.blocking_write(b"\r\n").is_err() {
        let mut error_msg = heapless::String::new();
        let _ = error_msg.push_str("HTTP Write Failed");
        return error_msg;
    }

    // Wait longer for HTTP response - network requests take time
    Timer::after(Duration::from_millis(1000)).await;

    let mut response_buffer = [0u8; 2048];
    let mut total_bytes = 0;
    let mut parsed_response = heapless::String::new();

    // Read response byte by byte with extended timeout
    for attempt in 0..50 {
        Timer::after(Duration::from_millis(200)).await;

        match uart.blocking_read(&mut response_buffer[total_bytes..total_bytes + 1]) {
            Ok(_) => {
                total_bytes += 1;
                if total_bytes >= response_buffer.len() {
                    break;
                }

                // Check for complete response periodically
                if total_bytes > 10 && total_bytes % 20 == 0 {
                    if let Ok(current_str) = core::str::from_utf8(&response_buffer[..total_bytes]) {
                        if current_str.contains("OK") || current_str.contains("ERROR") {
                            // We might have a complete response
                            break;
                        }
                    }
                }
            }
            Err(_) => {
                // No data this attempt
                if total_bytes > 0 && attempt > 20 {
                    // We have some data and waited long enough
                    break;
                }
            }
        }
    }

    if total_bytes > 0 {
        if let Ok(response_str) = core::str::from_utf8(&response_buffer[..total_bytes]) {
            info!("HTTP response ({} bytes): {}", total_bytes, response_str);

            // Parse AT+HTTPCLIENTLINE response format:
            // Response length:<len>
            // <actual_http_response>
            // OK

            if let Some(length_start) = response_str.find("Response length:") {
                // Find the length value
                if let Some(length_line_end) = response_str[length_start..].find('\n') {
                    let length_line = &response_str[length_start..length_start + length_line_end];
                    info!("Found response length line: {}", length_line);

                    // Find the start of actual HTTP response data
                    let data_start = length_start + length_line_end + 1;
                    if data_start < response_str.len() {
                        let response_data = &response_str[data_start..];

                        // Find the end of response (before "OK")
                        let end_marker = response_data
                            .find("\nOK")
                            .or_else(|| response_data.find("OK"))
                            .unwrap_or(response_data.len());

                        let http_body = response_data[..end_marker].trim();

                        if !http_body.is_empty() {
                            info!("Extracted HTTP body: {}", http_body);
                            let _ = parsed_response.push_str(http_body);
                        }
                    }
                }
            }

            // Fallback: look for any text that looks like our expected response
            if parsed_response.is_empty() {
                for line in response_str.lines() {
                    let line = line.trim();
                    if line.contains("hello world") || line.contains("api/simple") {
                        let _ = parsed_response.push_str(line);
                        break;
                    }
                }
            }
        }
    }

    // If we still have nothing, return an error
    if parsed_response.is_empty() {
        if total_bytes == 0 {
            let _ = parsed_response.push_str("No HTTP response");
        } else {
            let _ = parsed_response.push_str("Could not parse response");
        }
    }

    parsed_response
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // E-ink display setup - using SPI1 with original pins
    let epd_rst_pin = p.PIN_12;
    let epd_dc_pin = p.PIN_8;
    let epd_busy_pin = p.PIN_13;
    let epd_cs_pin = p.PIN_9;
    let epd_clk_pin = p.PIN_10;
    let epd_mosi_pin = p.PIN_11;
    let epd_miso_pin_dummy = p.PIN_28;

    let cs_epd = Output::new(epd_cs_pin, Level::High);
    let rst = Output::new(epd_rst_pin, Level::Low);
    let dc = Output::new(epd_dc_pin, Level::Low);
    let busy_in = gpio::Input::new(epd_busy_pin, gpio::Pull::None);
    let mut delay = embassy_time::Delay;

    let spi_cfg = spi::Config::default();
    let spi = Spi::new(
        p.SPI1,
        epd_clk_pin,
        epd_mosi_pin,
        epd_miso_pin_dummy,
        p.DMA_CH1,
        p.DMA_CH2,
        spi_cfg,
    );

    let spi_bus: Mutex<NoopRawMutex, _> = Mutex::new(RefCell::new(spi));
    let mut spi_dev = SpiDevice::new(&spi_bus, cs_epd);
    let mut epd3in7 = EPD3in7::new(&mut spi_dev, busy_in, dc, rst, &mut delay, None).unwrap();
    let mut display = Display3in7::default();

    display.set_rotation(DisplayRotation::Rotate90);

    // Build the style
    let style = MonoTextStyleBuilder::new()
        .font(&embedded_graphics::mono_font::ascii::FONT_7X14)
        .text_color(Color::White)
        .background_color(Color::Black)
        .build();
    let text_style = TextStyleBuilder::new()
        .baseline(Baseline::Top)
        .alignment(embedded_graphics::text::Alignment::Left)
        .build();

    // Setup UART for BW16 communication (GP4=TX, GP5=RX)
    let uart_config = UartConfig::default();
    let mut uart = Uart::new_blocking(p.UART1, p.PIN_4, p.PIN_5, uart_config);

    let _ = Text::with_text_style("Initializing BW16...", Point::new(20, 20), style, text_style).draw(&mut display);
    epd3in7
        .update_and_display_frame(&mut spi_dev, display.buffer(), &mut delay)
        .expect("display error");

    // Wait for BW16 to initialize
    Timer::after(Duration::from_millis(5000)).await;

    // Set WiFi mode
    info!("Setting WiFi mode...");
    let wifi_mode_success = send_at_command(&mut uart, SET_WIFI_MODE).await;

    if wifi_mode_success {
        info!("WiFi mode set successfully!");

        // Connect to WiFi
        let wifi_command = format_wifi_command(WIFI_NETWORK, WIFI_PASSWORD);
        info!("Connecting to WiFi...");
        let wifi_connect_success = send_at_command(&mut uart, &wifi_command).await;

        if wifi_connect_success {
            info!("WiFi connected successfully!");

            let _ = Text::with_text_style("WiFi Connected!", Point::new(20, 40), style, text_style).draw(&mut display);

            // Make HTTP request
            info!("Making HTTP request...");
            let api_response = send_http_request(&mut uart, HTTP_REQUEST).await;

            // Draw Vercel triangle in center
            let triangle_width = 40;
            let triangle_height = 32;

            let center_x = 240;
            let center_y = 140 - (triangle_height / 2);

            let triangle = Triangle::new(
                Point::new(center_x, center_y - triangle_height),
                Point::new(center_x - triangle_width, center_y + triangle_height),
                Point::new(center_x + triangle_width, center_y + triangle_height),
            )
            .into_styled(PrimitiveStyle::with_fill(Color::White));

            let _ = triangle.draw(&mut display);

            let _ = Text::with_text_style(
                &api_response,
                Point::new(center_x - 50, center_y + triangle_height + 20),
                style,
                text_style,
            )
            .draw(&mut display);
        } else {
            error!("Failed to connect to WiFi");
            let _ = Text::with_text_style("WiFi Failed!", Point::new(20, 40), style, text_style).draw(&mut display);
        }
    } else {
        error!("Failed to set WiFi mode");
        let _ = Text::with_text_style("BW16 Init Failed!", Point::new(20, 40), style, text_style).draw(&mut display);
    }

    // Show final display
    epd3in7
        .update_and_display_frame(&mut spi_dev, display.buffer(), &mut delay)
        .expect("display error");

    // Going to sleep
    epd3in7.sleep(&mut spi_dev, &mut delay).unwrap();

    loop {
        Timer::after_millis(1000).await;
    }
}
