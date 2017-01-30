use std::thread;
use std::process::exit;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures::{Stream, Future};
use self_meter::Meter;
use tk_easyloop;

use gossip;
use configs::Configs;
use stats::Stats;


quick_error! {
    #[derive(Debug)]
    pub enum InitError {
        Gossip(e: gossip::InitError) {
            from()
            display("error initializing gossip subsystem: {:?}", e)
        }
    }
}


fn spawn_self_scan(meter: Arc<Mutex<Meter>>) {
    tk_easyloop::handle().spawn(
        tk_easyloop::interval(Duration::new(1, 0)).for_each(move |()| {
            meter.lock().expect("meter is not poisoned")
            .scan()
            .map_err(|e| error!("Self-scan error: {}", e)).ok();
            Ok(())
        }).map_err(|_| -> () { unreachable!() }));
}


// All new async things should be in tokio main loop
pub fn start(gossip: gossip::Config,
    _configs: &Configs, stats: &Arc<RwLock<Stats>>,
    meter: &Arc<Mutex<Meter>>)
{
    let meter = meter.clone();
    let _stats = stats.clone();
    debug!("Starting tokio loop");

    thread::spawn(move || {
        meter.lock().unwrap().track_current_thread("tokio");
        tk_easyloop::run_forever(|| -> Result<(), InitError> {
            spawn_self_scan(meter);
            gossip::spawn(gossip)?;
            Ok(())
        }).map_err(|e| {
            error!("Error initializing tokio loop: {}", e);
            exit(1);
        }).expect("looping forever");
    });
}
