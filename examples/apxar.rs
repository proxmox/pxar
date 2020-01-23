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
    let mut i = 0;
    while let Some(entry) = reader.next().await {
        println!("{:#?}", entry.expect("failed to parse entry").path());
        i += 1;
        if i == 2 {
            break;
        }
    }

    // Use a Stream for the remaining entries:
    use futures::stream::StreamExt;

    let mut stream = reader.into_stream();

    while let Some(entry) = stream.next().await {
        println!("{:#?}", entry.expect("failed to parse entry").path());
    }
}
