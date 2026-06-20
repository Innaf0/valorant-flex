//! WiFi networking and Valorant API client.
//! Fetches match data from the HenrikDev Valorant API.

use core::str::from_utf8;

use cyw43::{aligned_bytes, JoinOptions};
use cyw43_pio::{PioSpi, DEFAULT_CLOCK_DIVIDER};
use defmt::*;
use embassy_executor::Spawner;
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_net::{Config, Stack, StackResources};
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::Peri;
use embassy_rp::{bind_interrupts, dma};

use heapless::{String, Vec};
use reqwless::client::HttpClient;
use reqwless::request::{Method, RequestBuilder};
use serde::Deserialize;
use serde_json_core::from_slice;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
});

const WIFI_NETWORK: &str = "ssid"; // change to your network SSID
const WIFI_PASSWORD: &str = "pwd"; // change to your network password

// Valorant API config
const API_KEY: &str = "YOUR_API_KEY"; // change to your HenrikDev API key
const PLAYER_REGION: &str = "eu"; // eu, na, ap, kr
const PLAYER_NAME: &str = "PlayerName"; // your Riot name
const PLAYER_TAG: &str = "TAG"; // your Riot tag (without #)

// --- Background tasks ---

#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

// --- Public types ---

/// Holds the initialized network stack and TLS seed, so fetch_match() can
/// be called repeatedly without re-initialising WiFi.
pub struct NetworkContext {
    pub stack: Stack<'static>,
    pub seed: u64,
}

/// Owned match summary (copied out of the JSON response).
#[derive(defmt::Format)]
pub struct MatchInfo {
    pub map: String<32>,
    pub mode: String<32>,
    pub game_start: String<64>,
    pub rounds_played: u32,
    pub region: String<8>,
    pub red_rounds_won: u32,
    pub blue_rounds_won: u32,
    pub red_has_won: bool,
    pub players: Vec<PlayerInfo, 10>,
}

/// Per-player stats for one match.
#[derive(defmt::Format)]
pub struct PlayerInfo {
    pub name: String<32>,
    pub tag: String<8>,
    pub team: String<8>,
    pub character: String<16>,
    pub rank: String<16>,
    pub kills: u32,
    pub deaths: u32,
    pub assists: u32,
    pub headshots: u32,
}

// --- Private serde types (borrowed from response buffer) ---

#[derive(Deserialize)]
struct MatchResponse<'a> {
    status: u16,
    #[serde(borrow)]
    data: Vec<MatchData<'a>, 1>,
}

#[derive(Deserialize)]
struct MatchData<'a> {
    #[serde(borrow)]
    metadata: MatchMetadata<'a>,
    #[serde(borrow)]
    players: PlayersData<'a>,
    teams: TeamsData,
}

#[derive(Deserialize)]
struct MatchMetadata<'a> {
    map: &'a str,
    mode: &'a str,
    #[serde(rename = "game_start_patched")]
    game_start: &'a str,
    rounds_played: u32,
    region: &'a str,
}

#[derive(Deserialize)]
struct PlayersData<'a> {
    #[serde(borrow)]
    all_players: Vec<PlayerSummary<'a>, 10>,
}

#[derive(Deserialize)]
struct PlayerSummary<'a> {
    name: &'a str,
    tag: &'a str,
    team: &'a str,
    character: &'a str,
    #[serde(rename = "currenttier_patched")]
    rank: &'a str,
    stats: PlayerStats,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct PlayerStats {
    score: u32,
    kills: u32,
    deaths: u32,
    assists: u32,
    headshots: u32,
    bodyshots: u32,
    legshots: u32,
}

#[derive(Deserialize)]
struct TeamsData {
    red: TeamResult,
    blue: TeamResult,
}

#[derive(Deserialize)]
struct TeamResult {
    has_won: bool,
    rounds_won: u32,
    #[allow(dead_code)]
    rounds_lost: u32,
}

// --- Public API ---

/// Initialise WiFi, connect to the network, obtain an IP address, and
/// spawn the background driver tasks.  Call this **once** at startup.
pub async fn init_network(
    spawner: Spawner,
    pin_23: Peri<'static, embassy_rp::peripherals::PIN_23>,
    pin_25: Peri<'static, embassy_rp::peripherals::PIN_25>,
    pin_24: Peri<'static, embassy_rp::peripherals::PIN_24>,
    pin_29: Peri<'static, embassy_rp::peripherals::PIN_29>,
    pio0: Peri<'static, embassy_rp::peripherals::PIO0>,
    dma_ch0: Peri<'static, embassy_rp::peripherals::DMA_CH0>,
) -> NetworkContext {
    info!("Initialising WiFi...");

    let mut rng = RoscRng;

    let fw = aligned_bytes!("./cyw43-firmware/43439A0.bin");
    let clm = aligned_bytes!("./cyw43-firmware/43439A0_clm.bin");
    let nvram = aligned_bytes!("./cyw43-firmware/nvram_rp2040.bin");

    let pwr = Output::new(pin_23, Level::Low);
    let cs = Output::new(pin_25, Level::High);
    let mut pio = Pio::new(pio0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        DEFAULT_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        pin_24,
        pin_29,
        dma::Channel::new(dma_ch0, Irqs),
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw, nvram).await;
    spawner.spawn(unwrap!(cyw43_task(runner)));

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    let config = Config::dhcpv4(Default::default());
    let seed = rng.next_u64();

    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        config,
        RESOURCES.init(StackResources::new()),
        seed,
    );

    spawner.spawn(unwrap!(net_task(runner)));

    while let Err(err) = control
        .join(WIFI_NETWORK, JoinOptions::new(WIFI_PASSWORD.as_bytes()))
        .await
    {
        info!("join failed: {:?}", err);
    }

    info!("waiting for link...");
    stack.wait_link_up().await;

    info!("waiting for DHCP...");
    stack.wait_config_up().await;

    info!("Network is up!");

    NetworkContext { stack, seed }
}

/// Fetch the latest match data from the Valorant API.
/// Returns `None` on any network / parse error.
pub async fn fetch_match(ctx: &NetworkContext) -> Option<MatchInfo> {
    let mut rx_buffer = [0; 4096];
    let mut tls_read_buffer = [0; 16640];
    let mut tls_write_buffer = [0; 16640];

    let client_state = TcpClientState::<1, 4096, 4096>::new();
    let tcp_client = TcpClient::new(ctx.stack, &client_state);
    let dns_client = DnsSocket::new(ctx.stack);
    let tls_config = reqwless::client::TlsConfig::new(
        ctx.seed,
        &mut tls_read_buffer,
        &mut tls_write_buffer,
        reqwless::client::TlsVerify::None,
    );

    let mut http_client = HttpClient::new_with_tls(&tcp_client, &dns_client, tls_config);

    let mut url_buf = [0u8; 128];
    let url = build_url(&mut url_buf, PLAYER_REGION, PLAYER_NAME, PLAYER_TAG);

    info!("Fetching: {}", url);

    let request = match http_client.request(Method::GET, url).await {
        Ok(req) => req,
        Err(e) => {
            error!("Failed to create request: {:?}", e);
            return None;
        }
    };

    let auth_headers = [("Authorization", API_KEY)];
    let mut request = request.headers(&auth_headers);
    let response = match request.send(&mut rx_buffer).await {
        Ok(resp) => resp,
        Err(e) => {
            error!("Failed to send request: {:?}", e);
            return None;
        }
    };

    info!("Response status: {}", response.status.0);

    let body_bytes = match response.body().read_to_end().await {
        Ok(b) => b,
        Err(_e) => {
            error!("Failed to read response body");
            return None;
        }
    };

    let body = match from_utf8(body_bytes) {
        Ok(b) => b,
        Err(_e) => {
            error!("Failed to parse body as UTF-8");
            return None;
        }
    };

    // Parse JSON and convert to owned MatchInfo
    let result = match from_slice::<MatchResponse>(body.as_bytes()) {
        Ok((output, _used)) => {
            if output.status != 200 {
                warn!("API returned non-200 status: {}", output.status);
            }
            output.data.first().map(|m| copy_match(m))
        }
        Err(e) => {
            error!("Failed to parse JSON: {}", Debug2Format(&e));
            let preview = if body.len() > 200 { &body[..200] } else { body };
            info!("Response preview: {:?}", preview);
            None
        }
    };
    result
}

// --- Helpers ---

/// Copy borrowed serde data into an owned MatchInfo.
fn copy_match(m: &MatchData<'_>) -> MatchInfo {
    fn copy_str<const N: usize>(s: &str) -> String<N> {
        let mut out = String::new();
        out.push_str(s).ok();
        out
    }

    let mut players: Vec<PlayerInfo, 10> = Vec::new();
    for p in &m.players.all_players {
        let _ = players.push(PlayerInfo {
            name: copy_str(p.name),
            tag: copy_str(p.tag),
            team: copy_str(p.team),
            character: copy_str(p.character),
            rank: copy_str(p.rank),
            kills: p.stats.kills,
            deaths: p.stats.deaths,
            assists: p.stats.assists,
            headshots: p.stats.headshots,
        });
    }

    MatchInfo {
        map: copy_str(m.metadata.map),
        mode: copy_str(m.metadata.mode),
        game_start: copy_str(m.metadata.game_start),
        rounds_played: m.metadata.rounds_played,
        region: copy_str(m.metadata.region),
        red_rounds_won: m.teams.red.rounds_won,
        blue_rounds_won: m.teams.blue.rounds_won,
        red_has_won: m.teams.red.has_won,
        players,
    }
}

/// Build a URL into the provided buffer.
fn build_url<'a>(buf: &'a mut [u8], region: &str, name: &str, tag: &str) -> &'a str {
    let base = b"https://api.henrikdev.xyz/valorant/v3/matches/";
    let mut pos = 0;

    buf[pos..pos + base.len()].copy_from_slice(base);
    pos += base.len();

    buf[pos..pos + region.len()].copy_from_slice(region.as_bytes());
    pos += region.len();
    buf[pos] = b'/';
    pos += 1;

    buf[pos..pos + name.len()].copy_from_slice(name.as_bytes());
    pos += name.len();
    buf[pos] = b'/';
    pos += 1;

    buf[pos..pos + tag.len()].copy_from_slice(tag.as_bytes());
    pos += tag.len();

    from_utf8(&buf[..pos])
        .unwrap_or("https://api.henrikdev.xyz/valorant/v3/matches/eu/PlayerName/TAG")
}
