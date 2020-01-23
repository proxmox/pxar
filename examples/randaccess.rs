use pxar::accessor::Accessor;

fn main() {
    let mut args = std::env::args_os().skip(1);

    let file = args.next().expect("expected a file name");

    let mut accessor = Accessor::open(file).expect("failed to open file");
    let mut dir = accessor
        .open_root()
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

    //    let file = tokio::fs::File::open(file)
    //        .await
    //        .expect("failed to open file");
    //
    //    let mut reader = Accessor::from_tokio(file)
    //        .await
    //        .expect("failed to open pxar archive contents");
    //    let mut i = 0;
    //    while let Some(entry) = reader.next().await {
    //        println!("{:#?}", entry.expect("failed to parse entry").path());
    //        i += 1;
    //        if i == 2 {
    //            break;
    //        }
    //    }
    //
    //    // Use a Stream for the remaining entries:
    //    use futures::stream::StreamExt;
    //
    //    let mut stream = reader.into_stream();
    //
    //    while let Some(entry) = stream.next().await {
    //        println!("{:#?}", entry.expect("failed to parse entry").path());
    //    }
}
