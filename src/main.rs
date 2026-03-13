mod config;
mod okx_interface;
mod instrument;
mod order_manager;

use config::Config;
use okx_interface::OkxInterface;
use instrument::Instrument;
use order_manager::OrderManager;

fn main() {
    println!("--- STARTUP ---");

    // 1. Config Loading
    let config = Config::load().expect("Failed to load config");
    println!("[1/4] Config Loaded. Mode: Paper Trading");

    // 2. Initialize Stack
    let interface = OkxInterface::new(config);
    let mut btc_swap = Instrument::new("BTC-USDT-SWAP");
    println!("[2/4] Interface & Instrument Initialized");

    // 3. Data Fetch (Hello World)
    println!("[3/4] Fetching Live Price...");
    if btc_swap.update(&interface) {
        println!("Current BTC Price: ${:.2}", btc_swap.last_price);
    } else {
        panic!("Failed to fetch initial data. Check VPN/Network.");
    }

    // 4. EXECUTE TRADE (The Grand Finale)
    println!("[4/4] Attempting Live Trade...");
    
    // Safety check: Don't drain wallet in a loop!
    // We buy 1 contract (usually $10-100 value depending on contract size)
    OrderManager::market_buy(&interface, &btc_swap.symbol, "1");

    println!("--- SHUTDOWN ---");
}