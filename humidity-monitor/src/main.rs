#![no_std]
#![no_main]

use core::fmt::Write;
use core::str::{self, FromStr};
use cyw43::JoinOptions;
use cyw43_pio::PioSpi;
use defmt::*;
use embassy_executor::Spawner;
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_net::{DhcpConfig, StackResources};
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{AnyPin, Level, Output, Pin};
use embassy_rp::i2c;
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio, PioPin};
use embassy_rp::{bind_interrupts, Peripheral, PeripheralRef};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel;
use embassy_time::Timer;
use heapless::String;
use rand::RngCore;
use reqwless::client::{HttpClient, TlsConfig, TlsVerify};
use reqwless::request::{Method, RequestBuilder};
use sensirion_rht::{Addr, Device, Repeatability};
use serde::{Deserialize, Serialize};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _, serde_json_core};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
});

#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

#[derive(Deserialize)]
struct Config<'a> {
    ssid: &'a str,
    password: &'a str,
    token: &'a str,
    thingsboard_url: &'a str,
}

#[derive(Serialize)]
struct Payload {
    temperature: f64,
    humidity: f64,
}

fn parse_config() -> Option<Config<'static>> {
    let config = include_bytes!("./config.json");
    match serde_json_core::de::from_slice::<Config>(config) {
        Ok((config, _used)) => Some(config),
        Err(_) => {
            error!("Failed to parse config");
            None
        }
    }
}

#[derive(Debug, Format)]
struct MeasurementResult {
    temperature: f64,
    humidity: f64,
}

static MEASUREMENT_CHANNEL: channel::Channel<CriticalSectionRawMutex, MeasurementResult, 1> =
    channel::Channel::new();

struct Pins {
    pwr: PeripheralRef<'static, AnyPin>,
    cs: PeripheralRef<'static, AnyPin>,
    pio: PeripheralRef<'static, PIO0>,

    dma: PeripheralRef<'static, DMA_CH0>,
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Hello! Starting up...");

    let p = embassy_rp::init(Default::default());

    let pwr_pin = AnyPin::from(p.PIN_23).into_ref();
    let cs_pin = AnyPin::from(p.PIN_25).into_ref();

    spawner
        .spawn(connect_and_send(
            spawner,
            Pins {
                pwr: pwr_pin,
                cs: cs_pin,
                pio: p.PIO0.into_ref(),
                dma: p.DMA_CH0.into_ref(),
            },
            p.PIN_24,
            p.PIN_29,
        ))
        .unwrap();

    let sender = MEASUREMENT_CHANNEL.sender();

    let sda = p.PIN_4;
    let scl = p.PIN_5;
    let i2c = i2c::I2c::new_blocking(p.I2C0, scl, sda, embassy_rp::i2c::Config::default());

    let mut sensor = Device::new_sht3x(Addr::A, i2c, embassy_time::Delay);
    loop {
        if let Ok((temperature, humidity)) = sensor.single_shot(Repeatability::High) {
            let result = MeasurementResult {
                temperature: temperature.as_celsius(),
                humidity: humidity.as_percent(),
            };

            sender.send(result).await;
        }

        Timer::after_millis(5000).await;
    }
}

#[embassy_executor::task]
async fn connect_and_send(spawner: Spawner, pins: Pins, p24: impl PioPin, p29: impl PioPin) {
    let config = unwrap!(parse_config());
    info!("Connecting to: {}", config.ssid);

    let fw = include_bytes!("../../cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../../cyw43-firmware/43439A0_clm.bin");

    let pwr = Output::new(pins.pwr, Level::Low);
    let cs = Output::new(pins.cs, Level::High);
    let mut pio = Pio::new(pins.pio, Irqs);
    let spi = PioSpi::new(&mut pio.common, pio.sm0, pio.irq0, cs, p24, p29, pins.dma);

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    unwrap!(spawner.spawn(cyw43_task(runner)));

    control.init(clm).await;

    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    // Connect to the network
    let mut dhcp_config: DhcpConfig = Default::default();
    dhcp_config.hostname = Some(heapless::String::from_str("humidity-monitor").unwrap());
    let net_config = embassy_net::Config::dhcpv4(dhcp_config);

    // Generate random seed
    let mut rng = RoscRng;
    let seed = rng.next_u64();

    // Init network stack
    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        net_config,
        RESOURCES.init(StackResources::new()),
        seed,
    );

    unwrap!(spawner.spawn(net_task(runner)));

    loop {
        match control
            .join(config.ssid, JoinOptions::new(config.password.as_bytes()))
            .await
        {
            Ok(_) => break,
            Err(err) => {
                info!("join failed with status={}", err.status);
            }
        }
    }

    // Wait for DHCP, not necessary when using static IP
    info!("waiting for DHCP...");
    while !stack.is_config_up() {
        Timer::after_millis(100).await;
    }
    info!("DHCP is now up!");

    info!("waiting for link up...");
    while !stack.is_link_up() {
        Timer::after_millis(500).await;
    }
    info!("Link is up!");

    info!("waiting for stack to be up...");
    stack.wait_config_up().await;
    info!("Stack is up!");

    let receiver = MEASUREMENT_CHANNEL.receiver();
    loop {
        let result = receiver.receive().await;
        info!("Received measurement: {:?}", result);

        let mut url: heapless::String<128> = heapless::String::new();
        core::write!(
            &mut url,
            "{}/api/v1/{}/telemetry",
            config.thingsboard_url,
            config.token,
        )
        .unwrap();

        let test = Payload {
            temperature: result.temperature,
            humidity: result.humidity,
        };

        let payload: String<128> = serde_json_core::ser::to_string(&test).unwrap();

        let mut rx_buffer = [0; 8192];
        let mut tls_read_buffer = [0; 16640];
        let mut tls_write_buffer = [0; 16640];

        let client_state = TcpClientState::<1, 4096, 4096>::new();
        let tcp_client = TcpClient::new(stack, &client_state);
        let dns_client = DnsSocket::new(stack);
        let tls_config = TlsConfig::new(
            seed,
            &mut tls_read_buffer,
            &mut tls_write_buffer,
            TlsVerify::None,
        );

        let mut http_client = HttpClient::new_with_tls(&tcp_client, &dns_client, tls_config);

        info!("Sending data to Thingsboard");

        let mut _request = http_client
            .request(Method::POST, &url)
            .await
            .unwrap()
            .headers(&[("Content-Type", "application/json")])
            .body(payload.as_bytes())
            .send(&mut rx_buffer)
            .await
            .unwrap();
    }
}
