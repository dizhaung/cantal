extern crate argparse;
extern crate cantal;

use std::error::Error;
use std::env;
use argparse::{ArgumentParser, List};
use cantal::Metadata;


fn main() {
    let mut files = Vec::<Path>::new();
    {
        let mut ap = ArgumentParser::new();
        ap.refer(&mut files)
            .add_argument("file", List,
                "List of `.values` files");
        match ap.parse_args() {
            Ok(()) => {}
            Err(x) => {
                env::set_exit_status(x);
                return;
            }
        }
    }
    for f in files.iter() {
        let meta = match Metadata::read(&f.with_extension("meta")) {
            Ok(meta) => meta,
            Err(e) => panic!("Error parsing metadata: {}", e),
        };
        let data = match meta.read_data(f) {
            Ok(data) => data,
            Err(e) => panic!("Error parsing data: {}", e.description()),
        };
        for &(ref descr, ref item) in data.iter() {
            println!("{:?} {} {:?}", f, descr.textname, item);
        }
    }
}
