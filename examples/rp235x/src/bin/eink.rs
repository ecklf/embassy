#![no_std]
#![no_main]

use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use embedded_graphics::mono_font::MonoTextStyleBuilder;
use embedded_graphics::text::Baseline;
use embedded_graphics::text::Text;
use embedded_graphics::text::TextStyleBuilder;
use epd_waveshare::epd4in2::Display4in2;
use static_cell::StaticCell;

use core::cell::RefCell;

use defmt::*;
use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio;
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_time::Timer;
use embedded_graphics::{
    prelude::*,
    primitives::{Line, PrimitiveStyle, Triangle},
};

use embassy_embedded_hal::shared_bus::blocking::spi::SpiDevice;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;

use embassy_rp::peripherals::{I2C1, SPI1};
use embassy_rp::spi;
use embassy_rp::spi::{Blocking, Spi};
use epd_waveshare::{epd3in7::*, prelude::*};

use gpio::{Level, Output};
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
});

type Spi1Bus = Mutex<NoopRawMutex, Spi<'static, SPI1, spi::Async>>;

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
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let epd_spi_pin = p.SPI1; // SPI peripheral
    let epd_rst_pin = p.PIN_12; // Reset pin
    let epd_dc_pin = p.PIN_8; // Data/Command pin
    let epd_busy_pin = p.PIN_13; // Busy status pin
    let epd_cs_pin = p.PIN_9; // SPI Chip Select pin
    let epd_clk_pin = p.PIN_10; // SPI Clock pin
    let epd_mosi_pin = p.PIN_11; // SPI Master Out Slave In pin
    let epd_miso_pin_dummy = p.PIN_24; // SPI Master In Slave Out pin (not used by EPD)

    let cs = Output::new(epd_cs_pin, Level::High);
    let rst = Output::new(epd_rst_pin, Level::Low);
    let dc = Output::new(epd_dc_pin, Level::Low);
    let busy_in = gpio::Input::new(epd_busy_pin, gpio::Pull::None);
    let mut delay = embassy_time::Delay;

    let spi_cfg = spi::Config::default();
    let spi = Spi::new(
        epd_spi_pin,
        epd_clk_pin,
        epd_mosi_pin,
        epd_miso_pin_dummy,
        p.DMA_CH0,
        p.DMA_CH1,
        spi_cfg,
    );

    let spi_bus: Mutex<NoopRawMutex, _> = Mutex::new(RefCell::new(spi));
    // static SPI_BUS: StaticCell<Spi1Bus> = StaticCell::new();
    // let spi_bus = SPI_BUS.init(Mutex::new(spi));

    let mut spi_dev = SpiDevice::new(&spi_bus, cs);
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

    // Draw "Hello from Rust" below the triangle
    let _ = Text::with_text_style(
        "Hello from Rust",
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

    // let mut pio = Pio::new(p.PIO0, Irqs);
    // let spi = PioSpi::new(
    //     &mut pio.common,
    //     pio.sm0,
    //     // SPI communication won't work if the speed is too high, so we use a divider larger than `DEFAULT_CLOCK_DIVIDER`.
    //     // See: https://github.com/embassy-rs/embassy/issues/3960.
    //     RM2_CLOCK_DIVIDER,
    //     pio.irq0,
    //     cs,
    //     p.PIN_24,
    //     p.PIN_29,
    //     p.DMA_CH0,
    // );

    // loop {
    //     info!("led on!");
    //     led.set_high();
    //     Timer::after_millis(250).await;
    //     info!("led off!");
    //     led.set_low();
    //     Timer::after_millis(250).await;
    // }
}
