//! This example uses the RP Pico 2 W board Wifi chip (cyw43).
//! Connects to Wifi network and makes a web request.
//!
//! NOTE: embedded-tls only supports TLS 1.3. Many servers (like httpbin.org) only support
//! TLS 1.2 and will fail with `Tls(HandshakeAborted(Warning, CloseNotify))`.
//! This example uses postman-echo.com which supports TLS 1.3.
//!
//! It does not work with the RP Pico 2 board. See other examples for non-wifi functionality.

#![no_std]
#![no_main]

extern crate alloc;

use core::str::from_utf8;
use embedded_alloc::LlffHeap as Heap;
use serde_json::Value;

#[global_allocator]
static HEAP: Heap = Heap::empty();

use cyw43::JoinOptions;
use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use defmt::*;
use embassy_executor::Spawner;
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_net::{Config, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_time::{Duration, Timer};
use reqwless::client::{HttpClient, TlsConfig, TlsVerify};
use reqwless::request::Method;

use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

// Program metadata for `picotool info`.
// This isn't needed, but it's recommended to have these minimal entries.
#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"WiFi Web Request Example"),
    embassy_rp::binary_info::rp_program_description!(
        c"This example connects to WiFi and makes HTTP requests to httpbin.org using the RP Pico 2 W."
    ),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
});

const WIFI_NETWORK: &str = "guest-01"; // change to your network SSID
const WIFI_PASSWORD: &str = "pickleswashere!"; // change to your network password

#[embassy_executor::task]
async fn cyw43_task(runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Hello World!");

    // Initialize the allocator (required for RSA signature verification in TLS)
    {
        use core::mem::MaybeUninit;
        const HEAP_SIZE: usize = 131072; // 128KB - needed for RSA + JSON parsing
        static HEAP_MEM: StaticCell<[MaybeUninit<u8>; HEAP_SIZE]> = StaticCell::new();
        let heap_mem = HEAP_MEM.init([MaybeUninit::uninit(); HEAP_SIZE]);
        unsafe { HEAP.init(heap_mem.as_ptr() as usize, HEAP_SIZE) }
    }

    let p = embassy_rp::init(Default::default());
    let mut rng = RoscRng;

    let fw = include_bytes!("../../../../cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../../../../cyw43-firmware/43439A0_clm.bin");
    // To make flashing faster for development, you may want to flash the firmwares independently
    // at hardcoded addresses, instead of baking them into the program with `include_bytes!`:
    //     probe-rs download ../../cyw43-firmware/43439A0.bin --binary-format bin --chip RP235x --base-address 0x10100000
    //     probe-rs download ../../cyw43-firmware/43439A0_clm.bin --binary-format bin --chip RP235x --base-address 0x10140000
    // let fw = unsafe { core::slice::from_raw_parts(0x10100000 as *const u8, 230321) };
    // let clm = unsafe { core::slice::from_raw_parts(0x10140000 as *const u8, 4752) };

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        // SPI communication won't work if the speed is too high, so we use a divider larger than `DEFAULT_CLOCK_DIVIDER`.
        // See: https://github.com/embassy-rs/embassy/issues/3960.
        RM2_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        p.DMA_CH0,
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    spawner.spawn(unwrap!(cyw43_task(runner)));

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    let config = Config::dhcpv4(Default::default());
    // Use static IP configuration instead of DHCP
    //let config = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
    //    address: Ipv4Cidr::new(Ipv4Address::new(192, 168, 69, 2), 24),
    //    dns_servers: Vec::new(),
    //    gateway: Some(Ipv4Address::new(192, 168, 69, 1)),
    //});

    // Generate random seed
    let seed = rng.next_u64();

    // Init network stack
    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(net_device, config, RESOURCES.init(StackResources::new()), seed);

    spawner.spawn(unwrap!(net_task(runner)));

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

    // And now we can use it!
    info!("Stack is up!");

    // And now we can use it!

    loop {
        let mut rx_buffer = [0; 16384];
        let mut tls_read_buffer = [0; 16640];
        let mut tls_write_buffer = [0; 4096];

        let client_state = TcpClientState::<1, 4096, 4096>::new();
        let tcp_client = TcpClient::new(stack, &client_state);
        let dns_client = DnsSocket::new(stack);
        let tls_config = TlsConfig::new(seed, &mut tls_read_buffer, &mut tls_write_buffer, TlsVerify::None);

        let mut http_client = HttpClient::new_with_tls(&tcp_client, &dns_client, tls_config);
        // NOTE: embedded-tls only supports TLS 1.3. httpbin.org uses TLS 1.2 and will fail.
        // Use postman-echo.com or jsonplaceholder.typicode.com which support TLS 1.3.
        let url = "https://api.brightsky.dev/weather?lat=52&lon=7.6&date=2020-04-21";

        info!("connecting to {}", &url);

        let mut request = match http_client.request(Method::GET, url).await {
            Ok(req) => req,
            Err(e) => {
                error!("Failed to make HTTP request: {:?}", e);
                Timer::after(Duration::from_secs(5)).await;
                continue;
            }
        };

        let response = match request.send(&mut rx_buffer).await {
            Ok(resp) => resp,
            Err(e) => {
                error!("Failed to send HTTP request: {:?}", e);
                Timer::after(Duration::from_secs(5)).await;
                continue;
            }
        };

        info!("Response status: {}", response.status.0);

        let body_bytes = match response.body().read_to_end().await {
            Ok(b) => b,
            Err(_e) => {
                error!("Failed to read response body");
                Timer::after(Duration::from_secs(5)).await;
                continue;
            }
        };

        let body = match from_utf8(body_bytes) {
            Ok(b) => b,
            Err(_e) => {
                error!("Failed to parse response body as UTF-8");
                Timer::after(Duration::from_secs(5)).await;
                continue;
            }
        };
        info!("Response body length: {} bytes", body.len());

        // Parse JSON using serde_json::Value (requires alloc)
        match serde_json::from_str::<Value>(body) {
            Ok(json) => {
                info!("Successfully parsed JSON!");
                // Pretty print isn't available without std, so we use Debug format
                info!("JSON: {}", Debug2Format(&json));
            }
            Err(e) => {
                error!("Failed to parse JSON: {}", Debug2Format(&e));
                let preview = if body.len() > 200 { &body[..200] } else { body };
                info!("Response preview: {:?}", preview);
            }
        }

        Timer::after(Duration::from_secs(5)).await;
    }
}
