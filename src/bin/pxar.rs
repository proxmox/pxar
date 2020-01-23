use pxar::decoder::Decoder;

fn main() {
    let mut args = std::env::args_os().skip(1);

    let file = args.next().expect("expected a file name");
    let file = std::fs::File::open(file).expect("failed to open file");

    let reader = Decoder::from_std(file).expect("failed to open pxar archive contents");
    for entry in reader {
        println!("{:#?}", entry.expect("failed to parse entry").path());
    }
}
