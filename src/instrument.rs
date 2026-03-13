use crate::okx_interface::OkxInterface;

pub struct Instrument {
    pub symbol: String,
    pub last_price: f64,
}

impl Instrument {
    pub fn new(symbol: &str) -> Self {
        Instrument {
            symbol: symbol.to_string(),
            last_price: 0.0,
        }
    }

    pub fn update(&mut self, interface: &OkxInterface) -> bool {
        match interface.get_ticker(&self.symbol) {
            Ok(json) => {
                // Parse safely using "if let" (Thesis: Pattern Matching)
                if let Some(data) = json["data"].as_array() {
                    if let Some(ticker) = data.first() {
                        if let Some(price_str) = ticker["last"].as_str() {
                            self.last_price = price_str.parse().unwrap_or(0.0);
                            return true;
                        }
                    }
                }
                false
            },
            Err(e) => {
                eprintln!("Ticker Update Failed: {}", e);
                false
            }
        }
    }
}