#![no_std]
#![no_main]

use core::cell::RefCell;
use core::str::from_utf8;

use cyw43::JoinOptions;
use cyw43_pio::DEFAULT_CLOCK_DIVIDER;
use cyw43_pio::PioSpi;
use embedded_graphics::mono_font::MonoTextStyleBuilder;
use embedded_graphics::text::Baseline;
use embedded_graphics::text::Text;
use embedded_graphics::text::TextStyleBuilder;

use static_cell::StaticCell;

use defmt::*;
use embassy_executor::Spawner;
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_net::{Config, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio;
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_time::Timer;
use embedded_graphics::{
    prelude::*,
    primitives::{PrimitiveStyle, Triangle},
};
use heapless;
use reqwless::client::HttpClient;
use reqwless::request::Method;

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

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
});

#[embassy_executor::task]
async fn cyw43_task(runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

// Program metadata for `picotool info`.
// This isn't needed, but it's recomended to have these minimal entries.
#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"Blinky Example"),
    embassy_rp::binary_info::rp_program_description!(
        c"This example tests the RP Pico on board LED, connected to gpio 25"
    ),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let mut rng = RoscRng;

    let fw = include_bytes!("../../../../cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../../../../cyw43-firmware/43439A0_clm.bin");

    // WiFi setup
    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs_wifi = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let pio_spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        DEFAULT_CLOCK_DIVIDER,
        pio.irq0,
        cs_wifi,
        p.PIN_24,
        p.PIN_29,
        p.DMA_CH0,
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, pio_spi, fw).await;
    spawner.spawn(unwrap!(cyw43_task(runner)));

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    // Network stack setup
    let config = Config::dhcpv4(Default::default());
    let seed = rng.next_u64();

    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(net_device, config, RESOURCES.init(StackResources::new()), seed);
    spawner.spawn(unwrap!(net_task(runner)));

    // Connect to WiFi
    while let Err(err) = control
        .join(WIFI_NETWORK, JoinOptions::new(WIFI_PASSWORD.as_bytes()))
        .await
    {
        info!("join failed with status={}", err.status);
    }

    info!("waiting for link...");
    stack.wait_link_up().await;

    info!("waiting for DHCP...");
    stack.wait_config_up().await;
    info!("Stack is up!");

    // Fetch data from web API
    let mut response_text = heapless::String::<256>::new();

    let mut rx_buffer = [0; 4096];
    let client_state = TcpClientState::<1, 4096, 4096>::new();
    let tcp_client = TcpClient::new(stack, &client_state);
    let dns_client = DnsSocket::new(stack);
    let mut http_client = HttpClient::new(&tcp_client, &dns_client);

    let url = "http://httpbin.org/json";
    info!("connecting to {}", &url);

    match http_client.request(Method::GET, url).await {
        Ok(mut request) => {
            match request.send(&mut rx_buffer).await {
                Ok(response) => {
                    info!("Response status: {}", response.status.0);
                    match response.body().read_to_end().await {
                        Ok(body_bytes) => {
                            match from_utf8(body_bytes) {
                                Ok(body) => {
                                    // Simple check for successful response
                                    if body.contains("slideshow") {
                                        let _ = response_text.push_str("HTTP Response OK");
                                    } else {
                                        let _ = response_text.push_str("Unexpected Response");
                                    }
                                }
                                Err(_) => {
                                    let _ = response_text.push_str("UTF-8 Error");
                                }
                            }
                        }
                        Err(_) => {
                            let _ = response_text.push_str("HTTP Body Error");
                        }
                    }
                }
                Err(_) => {
                    let _ = response_text.push_str("HTTP Send Error");
                }
            }
        }
        Err(_) => {
            let _ = response_text.push_str("HTTP Request Error");
        }
    }

    // E-ink display setup - using SPI1 with original pins
    // Note: PIO SPI for WiFi is independent from hardware SPI1
    let epd_rst_pin = p.PIN_12; // Reset pin
    let epd_dc_pin = p.PIN_8; // Data/Command pin
    let epd_busy_pin = p.PIN_13; // Busy status pin
    let epd_cs_pin = p.PIN_9; // SPI Chip Select pin
    let epd_clk_pin = p.PIN_10; // SPI Clock pin
    let epd_mosi_pin = p.PIN_11; // SPI Master Out Slave In pin
    let epd_miso_pin_dummy = p.PIN_28; // SPI Master In Slave Out pin

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
        p.DMA_CH1, // Different DMA channel from WiFi
        p.DMA_CH2, // Different DMA channel from WiFi
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
        .alignment(embedded_graphics::text::Alignment::Center)
        .build();

    // Draw Vercel triangle in center
    let triangle_width = 40;
    let triangle_height = 32;

    let center_x = 240; // 3.7" display is 480x280, so center is around 240
    let center_y = 140 - (triangle_height / 2); // Adjusting for text height

    let triangle = Triangle::new(
        Point::new(center_x, center_y - triangle_height), // Top point
        Point::new(center_x - triangle_width, center_y + triangle_height), // Bottom left
        Point::new(center_x + triangle_width, center_y + triangle_height), // Bottom right
    )
    .into_styled(PrimitiveStyle::with_fill(Color::White));

    let _ = triangle.draw(&mut display);

    // Draw web response below the triangle
    let _ = Text::with_text_style(
        response_text.as_str(),
        Point::new(center_x, center_y + triangle_height + 20),
        style,
        text_style,
    )
    .draw(&mut display);

    // Show display on e-paper
    epd3in7
        .update_and_display_frame(&mut spi_dev, display.buffer(), &mut delay)
        .expect("display error");

    // Going to sleep
    epd3in7.sleep(&mut spi_dev, &mut delay).unwrap();

    loop {
        Timer::after_millis(1000).await;
    }
}
