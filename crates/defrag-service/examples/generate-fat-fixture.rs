use std::{
    env,
    fs::OpenOptions,
    io::{Seek, SeekFrom, Write},
};

use fatfs::{FatType, FileSystem, FormatVolumeOptions, FsOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let path = args
        .next()
        .ok_or("usage: generate-fat-fixture IMAGE fat16|fat32")?;
    let variant = args.next().ok_or("missing FAT variant")?;
    if args.next().is_some() {
        return Err("too many arguments".into());
    }
    let (fat_type, length, cluster_size) = match variant.as_str() {
        "fat16" => (FatType::Fat16, 16 * 1024 * 1024, 1024),
        "fat32" => (FatType::Fat32, 40 * 1024 * 1024, 512),
        _ => return Err("variant must be fat16 or fat32".into()),
    };
    let mut disk = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    disk.set_len(length)?;
    fatfs::format_volume(
        &mut disk,
        FormatVolumeOptions::new()
            .fat_type(fat_type)
            .bytes_per_cluster(cluster_size),
    )?;
    disk.seek(SeekFrom::Start(0))?;
    let filesystem = FileSystem::new(disk, FsOptions::new())?;
    {
        let root = filesystem.root_dir();
        let mut target = root.create_file("fragmented payload.bin")?;
        let mut spacer = root.create_file("spacer.bin")?;
        let target_block = vec![0x5a; cluster_size as usize];
        let spacer_block = vec![0xa5; cluster_size as usize];
        for _ in 0..96 {
            target.write_all(&target_block)?;
            target.flush()?;
            spacer.write_all(&spacer_block)?;
            spacer.flush()?;
        }
        drop(target);
        drop(spacer);
        root.remove("spacer.bin")?;
        root.create_file("VFAT long filename.txt")?
            .write_all(b"long-name-data")?;
    }
    filesystem.unmount()?;
    Ok(())
}
