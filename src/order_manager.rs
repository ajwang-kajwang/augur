use crate::okx_interface::OkxInterface;

pub struct OrderManager;

impl OrderManager {
    pub fn market_buy(interface: &OkxInterface, symbol: &str, size: &str) {
        println!(">>> EXECUTING BUY ORDER: {} Contracts of {} <<<", size, symbol);
        
        match interface.place_order(symbol, "buy", size) {
            Ok(resp) => {
                if resp["code"] == "0" {
                    let ord_id = resp["data"][0]["ordId"].as_str().unwrap_or("Unknown");
                    println!("SUCCESS! Order ID: {}", ord_id);
                } else {
                    println!("ORDER REJECTED: {}", resp["msg"]);
                }
            },
            Err(e) => eprintln!("Execution Error: {}", e),
        }
    }
}