#![no_std]

//! Wi-Fi (station) + DHCP network bring-up shared by the waveshare ESP32-C6
//! apps. This crate only owns the identical boilerplate; all policy — when
//! to connect, what to fetch, when to disconnect — stays app-side.
//!
//! Credentials are passed in by the caller (each app reads its own
//! `WIFI_SSID` / `WIFI_PASS` build-time env).

use core::net::Ipv4Addr;
use embassy_executor::Spawner;
use embassy_net::{
    Config as NetConfig, Runner, Stack, StackResources,
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
};
use embassy_time::{Duration, Timer};
use esp_hal::{peripherals::WIFI, rng::Rng};
use esp_radio::wifi::{
    Config as WifiConfiguration, ControllerConfig, Interface, Ssid, WifiController,
    sta::StationConfig,
};
use reqwless::{client::HttpClient, request::Method};
use static_cell::StaticCell;

/// Brings up the Wi-Fi station and a DHCP-backed network stack, spawns the
/// embassy-net runner task, and returns the controller and stack for the
/// app's own connection/fetch policy task.
pub fn start_station(
    wifi_peripheral: WIFI<'static>,
    ssid: &'static str,
    password: &'static str,
    spawner: Spawner,
) -> (WifiController<'static>, Stack<'static>) {
    let station_config = StationConfig::default()
        .with_ssid(Ssid::from(ssid))
        .with_password(password.into());

    let controller_config =
        ControllerConfig::default().with_initial_config(WifiConfiguration::Station(station_config));

    let (controller, interfaces) =
        esp_radio::wifi::new(wifi_peripheral, controller_config).unwrap();

    static RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();

    let rng = Rng::new();
    let seed = (rng.random() as u64) | ((rng.random() as u64) << 32);

    let (stack, runner) = embassy_net::new(
        interfaces.station,
        NetConfig::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::<4>::new()),
        seed,
    );

    spawner.spawn(net_runner_task(runner).unwrap());

    (controller, stack)
}

#[embassy_executor::task]
async fn net_runner_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}

/// Joins the configured network with bounded retries, yielding `backoff`
/// between failed attempts. Returns true on association, false once the
/// attempts are exhausted.
pub async fn connect_with_retries(
    controller: &mut WifiController<'_>,
    max_attempts: usize,
    backoff: Duration,
) -> bool {
    for attempt in 0..max_attempts {
        if controller.connect_async().await.is_ok() {
            return true;
        }
        if attempt + 1 < max_attempts {
            Timer::after(backoff).await;
        }
    }
    false
}

/// Polls the stack for a DHCP-assigned address after association. Returns
/// the address as soon as it is up, or None after `max_polls` attempts
/// spaced `poll` apart.
pub async fn wait_for_ip(stack: &Stack<'_>, poll: Duration, max_polls: u32) -> Option<Ipv4Addr> {
    for _ in 0..max_polls {
        if stack.is_config_up()
            && let Some(config) = stack.config_v4()
        {
            return Some(config.address.address());
        }
        Timer::after(poll).await;
    }
    None
}

/// Performs a plain HTTP GET and returns the response body, read into `rx`.
///
/// `tcp_state` must be created once per app and must not be shared with
/// concurrent fetches. `TcpClientState<1, 2048, 2048>` is a comfortable
/// default socket sizing (it comfortably fits request headers plus a ~100
/// byte path); size `rx` to the largest expected body — the buffer stays
/// caller-owned so it is not paid for by apps with small payloads.
pub async fn http_get<'a, const N: usize, const TX: usize, const RXS: usize>(
    stack: Stack<'_>,
    tcp_state: &'static TcpClientState<N, TX, RXS>,
    rx: &'a mut [u8],
    url: &str,
) -> Option<&'a [u8]> {
    let tcp_client = TcpClient::new(stack, tcp_state);
    let dns = DnsSocket::new(stack);
    let mut client = HttpClient::new(&tcp_client, &dns);

    let mut request = client.request(Method::GET, url).await.ok()?;
    let response = request.send(rx).await.ok()?;
    let body = response.body().read_to_end().await.ok()?;
    Some(&*body)
}
