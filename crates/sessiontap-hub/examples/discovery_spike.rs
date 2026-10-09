//! Manual spike: announces the host's wildcard addresses for 8 s, then
//! withdraws. Watch with `avahi-browse -rpt _sessiontap._tcp`.
use std::time::Duration;

use sessiontap_hub::discovery::{Discovery, MdnsPublisher};
use sessiontap_hub::endpoints;

#[tokio::main]
async fn main() {
    let wildcard = "0.0.0.0".parse().unwrap();
    let discovery = Discovery::new(
        wildcard,
        Box::new(MdnsPublisher::new(wildcard)),
        endpoints::system_interfaces,
        Duration::from_secs(10),
    );
    let guard = discovery.announce("0.0.0.0:8932".parse().unwrap());
    tokio::time::sleep(Duration::from_secs(8)).await;
    println!("announced: {:?}", discovery.current());
    drop(guard);
    tokio::time::sleep(Duration::from_secs(2)).await;
    discovery.shutdown();
}
