#![no_std]
#![no_main]

use core::cell::RefCell;

use embedded_graphics::mono_font::MonoTextStyleBuilder;
use embedded_graphics::text::Baseline;
use embedded_graphics::text::Text;
use embedded_graphics::text::TextStyleBuilder;

use defmt::*;
use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio;
use embassy_rp::peripherals::UART1;
use embassy_rp::uart::{BufferedInterruptHandler, BufferedUart, Config as UartConfig};
use embassy_time::{Duration, Timer, with_timeout};
use embedded_graphics::{
    prelude::*,
    primitives::{PrimitiveStyle, Triangle},
};
use embedded_io_async::{Read, Write};
use heapless;
use static_cell::StaticCell;

use embassy_embedded_hal::shared_bus::blocking::spi::SpiDevice;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;

use embassy_rp::spi;
use embassy_rp::spi::Spi;
use epd_waveshare::{epd3in7::*, prelude::*};

use gpio::{Level, Output};
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    UART1_IRQ => BufferedInterruptHandler<UART1>;
});

const WIFI_NETWORK: &str = "guest-01";
const WIFI_PASSWORD: &str = "pickleswashere!";

const SET_WIFI_MODE: &str = "AT+WMODE=3,1";
const HTTP_REQUEST: &str =
    "AT+HTTPCLIENTLINE=2,2,\"application/json\",\"rust-fluid.vercel.app\",443,\"/api/json-example\"";

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

async fn send_at_command_with_timeout(uart: &mut BufferedUart, command: &str, timeout_ms: u64) -> bool {
    info!("Sending AT command: {}", command);

    // Send command
    if uart.write_all(command.as_bytes()).await.is_err() {
        error!("Failed to write command");
        return false;
    }
    if uart.write_all(b"\r\n").await.is_err() {
        error!("Failed to write CRLF");
        return false;
    }

    // Wait for and read response
    let mut response_buf = [0u8; 512];
    let mut total_bytes = 0;

    // Read response with timeout
    let timeout_duration = Duration::from_millis(timeout_ms);
    let read_result = with_timeout(timeout_duration, async {
        loop {
            let mut byte = [0u8; 1];
            match uart.read(&mut byte).await {
                Ok(1) => {
                    if total_bytes < response_buf.len() {
                        response_buf[total_bytes] = byte[0];
                        total_bytes += 1;
                    }
                    // Check if we have a complete response
                    if total_bytes >= 4 {
                        let response = &response_buf[..total_bytes];
                        if response.ends_with(b"OK\r\n") || response.ends_with(b"OK\n") {
                            return true;
                        }
                        if response.ends_with(b"ERROR\r\n")
                            || response.ends_with(b"ERROR\n")
                            || response.ends_with(b"FAIL\r\n")
                            || response.ends_with(b"FAIL\n")
                        {
                            return false;
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    error!("Read error: {:?}", e);
                    return false;
                }
            }
        }
    })
    .await;

    // Log what we received
    if total_bytes > 0 {
        if let Ok(s) = core::str::from_utf8(&response_buf[..total_bytes]) {
            info!("AT response ({} bytes): {}", total_bytes, s);
        } else {
            info!("AT response ({} bytes, non-UTF8)", total_bytes);
        }
    } else {
        info!("No response received");
    }

    match read_result {
        Ok(success) => success,
        Err(_) => {
            // Timeout - check if we got any positive indication
            if let Ok(s) = core::str::from_utf8(&response_buf[..total_bytes]) {
                s.contains("OK")
            } else {
                false
            }
        }
    }
}

async fn send_at_command(uart: &mut BufferedUart, command: &str) -> bool {
    send_at_command_with_timeout(uart, command, 5000).await
}

async fn send_http_request(uart: &mut BufferedUart, command: &str) -> heapless::String<256> {
    info!("Sending HTTP request: {}", command);

    // Send HTTP request command
    if uart.write_all(command.as_bytes()).await.is_err() {
        let mut error_msg = heapless::String::new();
        let _ = error_msg.push_str("HTTP Write Failed");
        return error_msg;
    }
    if uart.write_all(b"\r\n").await.is_err() {
        let mut error_msg = heapless::String::new();
        let _ = error_msg.push_str("HTTP Write Failed");
        return error_msg;
    }

    let mut response_buffer = [0u8; 2048];
    let mut total_bytes = 0;
    let mut parsed_response = heapless::String::new();

    // HTTP requests need longer timeout - network latency + TLS handshake
    let timeout_duration = Duration::from_secs(15);

    let read_result = with_timeout(timeout_duration, async {
        // Small delay to let the module start processing
        Timer::after(Duration::from_millis(100)).await;

        loop {
            let mut byte = [0u8; 1];
            match uart.read(&mut byte).await {
                Ok(1) => {
                    if total_bytes < response_buffer.len() {
                        response_buffer[total_bytes] = byte[0];
                        total_bytes += 1;
                    }

                    // Check for complete AT response
                    if total_bytes >= 4 {
                        let tail = &response_buffer[total_bytes.saturating_sub(6)..total_bytes];
                        // Check for OK\r\n or ERROR\r\n endings
                        if tail.ends_with(b"OK\r\n") || tail.ends_with(b"\nOK\n") {
                            return true;
                        }
                        if tail.ends_with(b"ERROR\r\n") || tail.ends_with(b"ERROR\n") {
                            return false;
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    info!("Read error: {:?}", e);
                    return false;
                }
            }
        }
    })
    .await;

    let success = match read_result {
        Ok(s) => s,
        Err(_) => {
            info!("HTTP request timed out after {} bytes", total_bytes);
            false
        }
    };

    if total_bytes > 0 {
        if let Ok(response_str) = core::str::from_utf8(&response_buffer[..total_bytes]) {
            info!(
                "HTTP response ({} bytes, success={}): {}",
                total_bytes, success, response_str
            );

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
                            .find("\r\nOK")
                            .or_else(|| response_data.find("\nOK"))
                            .unwrap_or(response_data.len());

                        let http_body = response_data[..end_marker].trim();

                        if !http_body.is_empty() {
                            info!("Extracted HTTP body: {}", http_body);
                            let _ = parsed_response.push_str(http_body);
                        }
                    }
                }
            }

            // Fallback: look for JSON-like content or expected response patterns
            if parsed_response.is_empty() {
                // Try to find JSON object
                if let Some(json_start) = response_str.find('{') {
                    if let Some(json_end) = response_str.rfind('}') {
                        if json_end > json_start {
                            let json_str = &response_str[json_start..=json_end];
                            let _ = parsed_response.push_str(json_str);
                        }
                    }
                }
            }

            // Secondary fallback: look for known text patterns
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
            let _ = parsed_response.push_str("Parse failed");
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
    // Using BufferedUart for proper async read/write
    static TX_BUF: StaticCell<[u8; 256]> = StaticCell::new();
    let tx_buf = &mut TX_BUF.init([0; 256])[..];
    static RX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
    let rx_buf = &mut RX_BUF.init([0; 2048])[..];

    let uart_config = UartConfig::default();
    let mut uart = BufferedUart::new(p.UART1, p.PIN_4, p.PIN_5, Irqs, tx_buf, rx_buf, uart_config);

    let _ = Text::with_text_style("Initializing BW16...", Point::new(20, 20), style, text_style).draw(&mut display);
    epd3in7
        .update_and_display_frame(&mut spi_dev, display.buffer(), &mut delay)
        .expect("display error");

    // Wait for BW16 to initialize
    Timer::after(Duration::from_millis(3000)).await;

    // First, send a simple AT command to check if the module is responding
    info!("Testing BW16 communication...");
    let at_test = send_at_command(&mut uart, "AT").await;
    if !at_test {
        error!("BW16 not responding to AT command");
    }

    // Set WiFi mode - use longer timeout
    info!("Setting WiFi mode...");
    let wifi_mode_success = send_at_command_with_timeout(&mut uart, SET_WIFI_MODE, 5000).await;

    if wifi_mode_success {
        info!("WiFi mode set successfully!");

        // Connect to WiFi - this can take 10-20 seconds
        let wifi_command = format_wifi_command(WIFI_NETWORK, WIFI_PASSWORD);
        info!("Connecting to WiFi (this may take up to 20 seconds)...");
        // WiFi connection needs a much longer timeout
        let wifi_connect_success = send_at_command_with_timeout(&mut uart, &wifi_command, 20000).await;

        if wifi_connect_success {
            info!("WiFi connected successfully!");

            // Give the module a moment to fully establish the connection
            Timer::after(Duration::from_millis(2000)).await;

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
