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

// API endpoint configuration
// Using httpbin.org with plain HTTP since BW16's TLS has issues
const API_HOST: &str = "httpbin.org";
const API_PATH: &str = "/json";
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

/// Send AT command and return the raw response
async fn send_at_command_get_response(
    uart: &mut BufferedUart,
    command: &str,
    timeout_ms: u64,
) -> heapless::String<512> {
    let mut response = heapless::String::new();

    info!("Sending: {}", command);

    // Send command
    if uart.write_all(command.as_bytes()).await.is_err() {
        let _ = response.push_str("WRITE_ERR");
        return response;
    }
    if uart.write_all(b"\r\n").await.is_err() {
        let _ = response.push_str("WRITE_ERR");
        return response;
    }

    // Read response
    let mut response_buf = [0u8; 512];
    let mut total_bytes = 0;

    let timeout_duration = Duration::from_millis(timeout_ms);
    let _ = with_timeout(timeout_duration, async {
        loop {
            let mut byte = [0u8; 1];
            match uart.read(&mut byte).await {
                Ok(1) => {
                    if total_bytes < response_buf.len() {
                        response_buf[total_bytes] = byte[0];
                        total_bytes += 1;
                    }
                    // Check for end of response
                    if total_bytes >= 4 {
                        let tail = &response_buf[..total_bytes];
                        if tail.ends_with(b"OK\r\n")
                            || tail.ends_with(b"OK\n")
                            || tail.ends_with(b"ERROR\r\n")
                            || tail.ends_with(b"ERROR\n")
                            || tail.ends_with(b"FAIL\r\n")
                            || tail.ends_with(b"FAIL\n")
                        {
                            return;
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    })
    .await;

    if let Ok(s) = core::str::from_utf8(&response_buf[..total_bytes]) {
        info!("Response: {}", s);
        let _ = response.push_str(s);
    }

    response
}

/// Make an HTTPS request using SSL socket
/// This uses AT+SOCKET=7 (SSLClient) for TLS connection
async fn send_https_request(uart: &mut BufferedUart, host: &str, path: &str) -> heapless::String<256> {
    let mut result = heapless::String::new();

    // Step 1: Create TCP socket connection (type 4 = TCPClient) to port 80
    // Using plain HTTP since BW16's TLS has issues with modern servers
    let mut socket_cmd = heapless::String::<128>::new();
    let _ = socket_cmd.push_str("AT+SOCKET=4,");
    let _ = socket_cmd.push_str(host);
    let _ = socket_cmd.push_str(",80");

    info!("Creating TCP socket: {}", socket_cmd.as_str());
    let socket_response = send_at_command_get_response(uart, &socket_cmd, 10000).await;

    // Check if connection succeeded - look for "connect success" or just "OK"
    if !socket_response.contains("OK") {
        let _ = result.push_str("TCP fail: ");
        // Add first 50 chars of response for debugging
        let snippet = if socket_response.len() > 50 {
            &socket_response[..50]
        } else {
            socket_response.as_str()
        };
        let _ = result.push_str(snippet);
        return result;
    }

    info!("TCP socket created successfully");

    // Give TLS handshake time to complete
    Timer::after(Duration::from_millis(1000)).await;

    // Step 2: Build and send HTTP GET request
    // Format: GET /path HTTP/1.1\r\nHost: hostname\r\nConnection: close\r\n\r\n
    let mut http_request = heapless::String::<256>::new();
    let _ = http_request.push_str("GET ");
    let _ = http_request.push_str(path);
    let _ = http_request.push_str(" HTTP/1.1\r\nHost: ");
    let _ = http_request.push_str(host);
    let _ = http_request.push_str("\r\nConnection: close\r\n\r\n");

    let request_len = http_request.len();

    // Use AT+SOCKETSEND to send the HTTP request
    let mut send_cmd = heapless::String::<32>::new();
    let _ = send_cmd.push_str("AT+SOCKETSEND=1,");
    // Format the length
    let mut len_str = heapless::String::<8>::new();
    let _ = core::fmt::write(&mut len_str, format_args!("{}", request_len));
    let _ = send_cmd.push_str(&len_str);

    info!("Sending HTTP request ({} bytes)", request_len);

    // Send the SOCKETSEND command
    if uart.write_all(send_cmd.as_bytes()).await.is_err() {
        let _ = result.push_str("Send cmd failed");
        return result;
    }
    if uart.write_all(b"\r\n").await.is_err() {
        let _ = result.push_str("Send cmd failed");
        return result;
    }

    // Wait for ">" prompt
    let mut prompt_buf = [0u8; 64];
    let mut prompt_len = 0;
    let prompt_timeout = Duration::from_millis(3000);

    let got_prompt = with_timeout(prompt_timeout, async {
        loop {
            let mut byte = [0u8; 1];
            if uart.read(&mut byte).await.is_ok() {
                if prompt_len < prompt_buf.len() {
                    prompt_buf[prompt_len] = byte[0];
                    prompt_len += 1;
                }
                if byte[0] == b'>' {
                    return true;
                }
            }
        }
    })
    .await;

    if got_prompt.is_err() || !got_prompt.unwrap() {
        let _ = result.push_str("No > prompt");
        return result;
    }

    // Send the actual HTTP request data
    if uart.write_all(http_request.as_bytes()).await.is_err() {
        let _ = result.push_str("HTTP send failed");
        return result;
    }

    // Wait for send confirmation
    Timer::after(Duration::from_millis(500)).await;

    // Step 3: Read response using AT+SOCKETREAD or wait for +EVENT:SocketDown
    // First, enable active receive mode
    let _ = send_at_command(uart, "AT+SOCKETRECVCFG=1").await;

    // Wait for response data
    let mut response_buffer = [0u8; 2048];
    let mut total_bytes = 0;

    let read_timeout = Duration::from_secs(10);
    let _ = with_timeout(read_timeout, async {
        loop {
            let mut byte = [0u8; 1];
            match uart.read(&mut byte).await {
                Ok(1) => {
                    if total_bytes < response_buffer.len() {
                        response_buffer[total_bytes] = byte[0];
                        total_bytes += 1;
                    }

                    // Check if we've received a complete HTTP response
                    // Look for end of HTTP response or socket close event
                    if total_bytes > 10 {
                        let tail = &response_buffer[..total_bytes];
                        // Check for various end conditions
                        if tail.ends_with(b"\r\n\r\n") || tail.ends_with(b"}\r\n") || tail.ends_with(b"}\n") {
                            // Might have complete JSON response
                            if let Ok(s) = core::str::from_utf8(tail) {
                                if s.contains('}') && s.matches('{').count() == s.matches('}').count() {
                                    return;
                                }
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    })
    .await;

    info!("Received {} bytes", total_bytes);

    // Parse the response
    if total_bytes > 0 {
        if let Ok(response_str) = core::str::from_utf8(&response_buffer[..total_bytes]) {
            info!("Raw response: {}", response_str);

            // Look for JSON in the response
            if let Some(json_start) = response_str.find('{') {
                if let Some(json_end) = response_str.rfind('}') {
                    if json_end > json_start {
                        let json_str = &response_str[json_start..=json_end];
                        let _ = result.push_str(json_str);
                    }
                }
            }

            // If no JSON found, return a snippet
            if result.is_empty() {
                let snippet = if response_str.len() > 250 {
                    &response_str[..250]
                } else {
                    response_str
                };
                let _ = result.push_str(snippet);
            }
        }
    }

    // Step 4: Close the socket
    let _ = send_at_command(uart, "AT+SOCKETDEL=1").await;

    if result.is_empty() {
        let _ = result.push_str("No response data");
    }

    result
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

    // Disable command echo so responses don't include the command
    info!("Disabling echo...");
    let _ = send_at_command(&mut uart, "ATE0").await;

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

            // Make HTTPS request using SSL socket
            info!("Making HTTPS request...");
            let api_response = send_https_request(&mut uart, API_HOST, API_PATH).await;

            let _ = Text::with_text_style(&api_response, Point::new(20, 60), style, text_style).draw(&mut display);
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
