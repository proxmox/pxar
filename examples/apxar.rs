use pxar::decoder::aio::Decoder;

#[tokio::main]
async fn main() {
    let mut args = std::env::args_os().skip(1);

    let file = args.next().expect("expected a file name");
    let file = tokio::fs::File::open(file)
        .await
        .expect("failed to open file");

    let mut reader = Decoder::from_tokio(file)
        .await
        .expect("failed to open pxar archive contents");

    while let Some(entry) = reader.next().await {
        println!("{:#?}", entry.expect("failed to parse entry").path());
    }
}
