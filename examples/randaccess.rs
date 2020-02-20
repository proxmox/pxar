use std::sync::Arc;

use pxar::accessor::Accessor;

fn main() {
    let mut args = std::env::args_os().skip(1);

    let file = args.next().expect("expected a file name");
    let file = std::fs::File::open(file).expect("failed to open file");
    let file = Arc::new(file);

    let accessor = Accessor::from_file_ref(file).expect("failed to open file");
    let dir = accessor
        .open_root_ref()
        .expect("failed to open archive root directory");
    for i in dir.decode_full().expect("failed to access root directory") {
        println!("{:#?}", i.expect("failed to parse entry").path());
    }

    let da = dir
        .lookup("da")
        .expect("error looking up da/")
        .expect("failed to lookup da/");
    dir.lookup("db").expect("failed to lookup db");
    dir.lookup("root1.txt").expect("failed to lookup root1.txt");
    dir.lookup("root2.txt").expect("failed to lookup root2.txt");

    println!("{:?}", da.entry());
    let da = da.enter_directory().expect("failed to enter /da directory");
    for i in da.decode_full().expect("failed to access /da directory") {
        println!(
            " ==> {:#?}",
            i.expect("failed to parse /da file entry").path()
        );
    }

    for i in dir.read_dir() {
        let i = i.expect("failed to read directory entry");
        println!("read_dir => {:?}", i.file_name());
    }
}
