use crate::file_data_source::FileDataSource;
use anyhow::Context;
use fatfs::Dir;
use std::fs::File;
use std::{collections::BTreeMap, fs, path::Path};

use crate::KERNEL_FILE_NAME;

pub fn create_fat_filesystem(
    files: BTreeMap<&str, &FileDataSource>,
    out_fat_path: &Path,
) -> anyhow::Result<()> {
    const MB: u64 = 1024 * 1024;

    // calculate needed size
    let mut needed_size = 0;
    for source in files.values() {
        needed_size += source.len()?;
    }

    // create new filesystem image file at the given path and set its length
    let fat_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(out_fat_path)
        .unwrap();
    let fat_size_padded_and_rounded = ((needed_size + 1024 * 64 - 1) / MB + 1) * MB + MB;
    fat_file.set_len(fat_size_padded_and_rounded).unwrap();
    // BIOS stage 2 reads a FAT12/FAT16 root directory. A volume of 512 MiB
    // or more is formatted as FAT32 with the default cluster size, and
    // stage 2 then hits `unimplemented!()` in `fat.rs`. Pick a cluster
    // size that keeps the cluster count inside FAT16.
    let bytes_per_cluster = fat16_cluster_bytes(fat_size_padded_and_rounded)?;

    // choose a file system label
    let mut label = *b"MY_RUST_OS!";

    // This __should__ always be a file, but maybe not. Should we allow the caller to set the volume label instead?
    if let Some(FileDataSource::File(path)) = files.get(KERNEL_FILE_NAME)
        && let Some(name) = path.file_stem()
    {
        let converted = name.to_string_lossy();
        let name = converted.as_bytes();
        let mut new_label = [0u8; 11];
        let name = &name[..usize::min(new_label.len(), name.len())];
        let slice = &mut new_label[..name.len()];
        slice.copy_from_slice(name);
        label = new_label;
    }

    // format the file system and open it
    let format_options = fatfs::FormatVolumeOptions::new()
        .volume_label(label)
        .bytes_per_cluster(bytes_per_cluster);
    fatfs::format_volume(&fat_file, format_options).context("Failed to format FAT file")?;
    let filesystem = fatfs::FileSystem::new(&fat_file, fatfs::FsOptions::new())
        .context("Failed to open FAT file system of UEFI FAT file")?;
    let root_dir = filesystem.root_dir();

    // copy files to file system
    add_files_to_image(&root_dir, files)
}

/// Cluster size that keeps a volume in FAT12 or FAT16.
///
/// `fatfs` tries FAT32 first. With 512-byte or 4 KiB clusters a desk image
/// (userspace plus `rustc` and `python`) has at least 65525 clusters, so
/// the formatter picks FAT32. BIOS stage 2 does not implement that root
/// directory. FAT16 holds at most 65524 clusters, and the largest cluster
/// fatfs will write is 32 KiB, which covers a volume just under 2 GiB.
fn fat16_cluster_bytes(volume_len: u64) -> anyhow::Result<u32> {
    const MAX_CLUSTERS: u64 = 65524;
    const MAX_CLUSTER: u32 = 32 * 1024;
    let mut cluster: u32 = 512;
    loop {
        // FAT and root-directory overhead only lower the cluster count.
        if volume_len / u64::from(cluster) < MAX_CLUSTERS {
            return Ok(cluster);
        }
        if cluster >= MAX_CLUSTER {
            anyhow::bail!(
                "FAT volume of {volume_len} bytes does not fit in FAT16 \
                 ({MAX_CLUSTERS} clusters of {MAX_CLUSTER} bytes); BIOS stage 2 \
                 cannot read a FAT32 root directory"
            );
        }
        cluster *= 2;
    }
}

pub fn add_files_to_image(
    root_dir: &Dir<&File>,
    files: BTreeMap<&str, &FileDataSource>,
) -> anyhow::Result<()> {
    for (target_path_raw, source) in files {
        let target_path = Path::new(target_path_raw);
        // create parent directories
        let ancestors: Vec<_> = target_path.ancestors().skip(1).collect();
        for ancestor in ancestors.into_iter().rev().skip(1) {
            root_dir
                .create_dir(&ancestor.display().to_string())
                .with_context(|| {
                    format!(
                        "failed to create directory `{}` on FAT filesystem",
                        ancestor.display()
                    )
                })?;
        }

        let mut new_file = root_dir
            .create_file(target_path_raw)
            .with_context(|| format!("failed to create file at `{}`", target_path.display()))?;
        new_file.truncate().unwrap();

        source.copy_to(&mut new_file).with_context(|| {
            format!(
                "failed to copy source data `{:?}` to file at `{}`",
                source,
                target_path.display()
            )
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::fat16_cluster_bytes;
    use fatfs::{FileSystem, FormatVolumeOptions, FsOptions, format_volume};
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom};

    /// Same cluster count `bootloader-x86_64-bios-stage-2` uses to choose
    /// FAT32 and then `unimplemented!()` the root directory.
    fn stage2_cluster_count(boot: &[u8]) -> u32 {
        let bytes_per_sector = u16::from_le_bytes(boot[11..13].try_into().unwrap());
        let sectors_per_cluster = boot[13];
        let reserved_sector_count = u16::from_le_bytes(boot[14..16].try_into().unwrap());
        let num_fats = boot[16];
        let root_entry_count = u16::from_le_bytes(boot[17..19].try_into().unwrap());
        let total_sectors_16 = u16::from_le_bytes(boot[19..21].try_into().unwrap());
        let fat_size_16 = u16::from_le_bytes(boot[22..24].try_into().unwrap());
        let total_sectors_32 = u32::from_le_bytes(boot[32..36].try_into().unwrap());
        let fat_size_32 = if fat_size_16 == 0 {
            u32::from_le_bytes(boot[36..40].try_into().unwrap())
        } else {
            0
        };
        let fat_size_in_sectors = if fat_size_16 != 0 && fat_size_32 == 0 {
            u32::from(fat_size_16)
        } else {
            fat_size_32
        };
        let root_dir_sectors = (root_entry_count as u32 * 32).div_ceil(u32::from(bytes_per_sector));
        let total_sectors = if total_sectors_16 != 0 {
            u32::from(total_sectors_16)
        } else {
            total_sectors_32
        };
        let data_sectors = total_sectors
            - (u32::from(reserved_sector_count)
                + (u32::from(num_fats) * fat_size_in_sectors)
                + root_dir_sectors);
        data_sectors / u32::from(sectors_per_cluster)
    }

    fn format_at(len: u64, cluster: Option<u32>) -> (fatfs::FatType, u32) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vol");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .unwrap();
        file.set_len(len).unwrap();
        let mut opts = FormatVolumeOptions::new();
        if let Some(cluster) = cluster {
            opts = opts.bytes_per_cluster(cluster);
        }
        format_volume(&mut file, opts).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut boot = [0u8; 512];
        file.read_exact(&mut boot).unwrap();
        let clusters = stage2_cluster_count(&boot);
        file.seek(SeekFrom::Start(0)).unwrap();
        let fs = FileSystem::new(&file, FsOptions::new()).unwrap();
        (fs.fat_type(), clusters)
    }

    #[test]
    fn cluster_budget_matches_fat16_limits() {
        assert_eq!(fat16_cluster_bytes(1024 * 1024).unwrap(), 512);
        assert_eq!(fat16_cluster_bytes(512 * 1024 * 1024).unwrap(), 16 * 1024);
        assert!(fat16_cluster_bytes(2 * 1024 * 1024 * 1024).is_err());
    }

    #[test]
    fn desk_sized_volume_is_fat16_for_stage2() {
        const LEN: u64 = 512 * 1024 * 1024;
        let (default_ty, default_clusters) = format_at(LEN, None);
        assert_eq!(default_ty, fatfs::FatType::Fat32, "{default_clusters}");
        assert!(
            default_clusters >= 65525,
            "default 512 MiB volume had {default_clusters} clusters"
        );

        let cluster = fat16_cluster_bytes(LEN).unwrap();
        let (ty, clusters) = format_at(LEN, Some(cluster));
        assert_eq!(ty, fatfs::FatType::Fat16);
        assert!(
            (4085..65525).contains(&clusters),
            "stage 2 would see {clusters} clusters"
        );
    }
}
