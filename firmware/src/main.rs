#![no_std]
#![no_main]
mod request;
use defmt::*;
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use {defmt_rtt as _, panic_probe as _};

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // Initialise WiFi once
    let ctx = request::init_network(
        spawner, p.PIN_23, p.PIN_25, p.PIN_24, p.PIN_29, p.PIO0, p.DMA_CH0,
    )
    .await;

    loop {
        match request::fetch_match(&ctx).await {
            Some(m) => {
                info!("=== Match ===");
                info!("Map:         {}", m.map.as_str());
                info!("Mode:        {}", m.mode.as_str());
                info!("Region:      {}", m.region.as_str());
                info!("Started:     {}", m.game_start.as_str());
                info!("Rounds:      {}", m.rounds_played);
                info!(
                    "Score:       Red {} - {} Blue",
                    m.red_rounds_won, m.blue_rounds_won,
                );
                info!(
                    "Winner:      {}",
                    if m.red_has_won { "Red" } else { "Blue" }
                );

                info!("=== Players ===");
                for player in &m.players {
                    info!(
                        "  [{}] {} #{} | {} | {} | K/D/A: {}/{}/{} | HS: {}",
                        player.team.as_str(),
                        player.name.as_str(),
                        player.tag.as_str(),
                        player.character.as_str(),
                        player.rank.as_str(),
                        player.kills,
                        player.deaths,
                        player.assists,
                        player.headshots,
                    );
                }
            }
            None => {
                warn!("Failed to fetch match data, retrying...");
            }
        }

        Timer::after(Duration::from_secs(30)).await;
    }
}
