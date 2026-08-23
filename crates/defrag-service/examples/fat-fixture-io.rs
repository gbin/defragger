use std::{env, fs, io::Write, path::Path};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let operation = args
        .next()
        .ok_or("usage: fat-fixture-io populate|verify MOUNT")?;
    let mount = args.next().ok_or("missing mount path")?;
    if args.next().is_some() {
        return Err("too many arguments".into());
    }
    match operation.as_str() {
        "populate" => populate(Path::new(&mount)),
        "verify" => verify(Path::new(&mount)),
        _ => Err("operation must be populate or verify".into()),
    }
}

fn populate(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut target = fs::File::create(root.join("fragmented payload.bin"))?;
    let mut spacer = fs::File::create(root.join("spacer.bin"))?;
    let target_block = [0x5a; 4096];
    let spacer_block = [0xa5; 4096];
    for _ in 0..96 {
        target.write_all(&target_block)?;
        target.sync_data()?;
        spacer.write_all(&spacer_block)?;
        spacer.sync_data()?;
    }
    drop(target);
    drop(spacer);
    fs::remove_file(root.join("spacer.bin"))?;

    let nested = root.join("Directory created by Linux VFAT");
    fs::create_dir(&nested)?;
    fs::write(
        nested.join("Nested VFAT long filename.txt"),
        b"nested-long-name-data",
    )?;
    fs::write(
        root.join("Root VFAT long filename.txt"),
        b"root-long-name-data",
    )?;
    sync_mount(root)?;
    Ok(())
}

fn verify(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let payload = fs::read(root.join("fragmented payload.bin"))?;
    if payload.len() != 96 * 4096 || !payload.iter().all(|byte| *byte == 0x5a) {
        return Err("fragmented payload changed".into());
    }
    let nested =
        fs::read(root.join("Directory created by Linux VFAT/Nested VFAT long filename.txt"))?;
    if nested != b"nested-long-name-data" {
        return Err("nested VFAT long-name payload changed".into());
    }
    if fs::read(root.join("Root VFAT long filename.txt"))? != b"root-long-name-data" {
        return Err("root VFAT long-name payload changed".into());
    }
    Ok(())
}

fn sync_mount(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let directory = fs::File::open(root)?;
    directory.sync_all()?;
    Ok(())
}
