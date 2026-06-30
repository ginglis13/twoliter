//! PCR prediction tool for Bottlerocket.
//!
//! Predicts TPM Platform Configuration Register (PCR) values based on
//! boot components, EFI variables, and GPT partition tables.

mod aws;
mod diskfs;
mod efi;
mod error;
mod gpt;
mod parsers;
mod pcrs;
mod pe;
mod platform;
mod predict;
mod schnauzer;

use aws_config::profile::ProfileFileCredentialsProvider;
use aws_types::region::Region;
use aws_types::SdkConfig;
use clap::{Parser, Subcommand};
use coldsnap::SnapshotDownloader;
use snafu::prelude::*;
use std::fs;
use std::path::PathBuf;

use crate::error::Result;
use crate::platform::Platform;
use crate::predict::{PcrContext, PcrPredictions};

/// Command-line arguments for pcrsys.
#[derive(Parser)]
#[command(version, about = "Predict TPM PCR values for Bottlerocket")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

/// Subcommands for PCR prediction from different sources.
#[derive(Subcommand)]
enum Command {
    /// Predict PCRs from a local disk image
    Disk {
        /// Path to disk image containing GPT, ESP, and boot partitions
        #[arg(long)]
        image: PathBuf,

        /// Path to efi-vars.json containing Secure Boot variables
        #[arg(long)]
        efi_vars: PathBuf,

        /// Target platform (aws, vmware, metal)
        #[arg(long, value_enum, default_value_t = Platform::Aws)]
        platform: Platform,

        /// Path to user-data TOML file for PCR 8 prediction
        #[arg(long)]
        user_data: Option<PathBuf>,

        /// AWS region for simulating schnauzer templates (required when --user-data is provided)
        #[arg(long)]
        region: Option<String>,
    },

    /// Predict PCRs from an AWS AMI
    Ami {
        /// AMI ID (e.g., ami-0123456789abcdef0)
        #[arg(long)]
        ami_id: String,

        /// AWS region to use
        #[arg(long)]
        region: Option<String>,

        /// AWS profile to use
        #[arg(long)]
        profile: Option<String>,

        /// Path to user-data TOML file for PCR 8 prediction
        #[arg(long)]
        user_data: Option<PathBuf>,
    },
}

#[snafu::report]
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    match &args.command {
        Command::Disk {
            image,
            efi_vars,
            platform,
            user_data,
            region,
        } => run_disk(image, efi_vars, *platform, user_data.as_ref(), region.as_deref()).await,
        Command::Ami {
            ami_id,
            region,
            profile,
            user_data,
        } => {
            run_ami(
                ami_id,
                region.as_ref(),
                profile.as_ref(),
                user_data.as_ref(),
            )
            .await
        }
    }
}

/// Run PCR prediction from a local disk image file.
async fn run_disk(
    image: &PathBuf,
    efi_vars_path: &PathBuf,
    platform: Platform,
    user_data_path: Option<&PathBuf>,
    region: Option<&str>,
) -> Result<()> {
    let efi_vars_json = fs::read_to_string(efi_vars_path).whatever_context(format!(
        "failed to read efi-vars: {}",
        efi_vars_path.display()
    ))?;

    let user_data = match user_data_path {
        Some(path) => Some(
            fs::read_to_string(path)
                .whatever_context(format!("failed to read user-data: {}", path.display()))?,
        ),
        None => None,
    };

    let mut disk = fs::File::open(image)
        .whatever_context(format!("failed to open disk image: {}", image.display()))?;

    let efi_vars: efi::EfiVars =
        serde_json::from_str(&efi_vars_json).whatever_context("failed to parse efi-vars.json")?;

    let predictions = predict_pcrs(&efi_vars, &mut disk, image, platform, user_data.as_deref(), region).await?;
    let json = serde_json::to_string_pretty(&predictions)
        .whatever_context("failed to serialize predictions")?;
    println!("{json}");

    Ok(())
}

/// Run PCR prediction from an AWS AMI by downloading its snapshot.
async fn run_ami(
    ami_id: &str,
    region: Option<&String>,
    profile: Option<&String>,
    user_data_path: Option<&PathBuf>,
) -> Result<()> {
    let config = build_client_config(region, profile).await;

    let ec2_client = aws_sdk_ec2::Client::new(&config);
    let ebs_client = aws_sdk_ebs::Client::new(&config);

    let efi_vars = aws::ami::get_uefi_data(&ec2_client, ami_id).await?;

    let snapshot_id = aws::ami::get_root_snapshot_id(&ec2_client, ami_id).await?;

    let user_data = match user_data_path {
        Some(path) => Some(
            fs::read_to_string(path)
                .whatever_context(format!("failed to read user-data: {}", path.display()))?,
        ),
        None => None,
    };

    let temp_file =
        tempfile::NamedTempFile::new().whatever_context("failed to create temp file")?;

    let downloader = SnapshotDownloader::new(ebs_client);
    downloader
        .download_to_file(&snapshot_id, temp_file.path(), None)
        .await
        .whatever_context("failed to download snapshot")?;

    let mut disk =
        fs::File::open(temp_file.path()).whatever_context("failed to open downloaded snapshot")?;

    let predictions = predict_pcrs(
        &efi_vars,
        &mut disk,
        temp_file.path(),
        Platform::Aws,
        user_data.as_deref(),
        region.map(|s| s.as_str()),
    )
    .await?;
    let json = serde_json::to_string_pretty(&predictions)
        .whatever_context("failed to serialize predictions")?;
    println!("{json}");

    Ok(())
}

/// Build AWS SDK config, handling region and profile options like coldsnap.
async fn build_client_config(region: Option<&String>, profile: Option<&String>) -> SdkConfig {
    let config = match (region, profile) {
        (Some(r), _) => aws_config::from_env().region(Region::new(r.clone())),
        (None, Some(p)) => aws_config::from_env().region(
            aws_config::profile::ProfileFileRegionProvider::builder()
                .profile_name(p)
                .build(),
        ),
        (None, None) => aws_config::from_env(),
    };

    let config = match profile {
        Some(p) => config.credentials_provider(
            ProfileFileCredentialsProvider::builder()
                .profile_name(p)
                .build(),
        ),
        None => config,
    };

    config.load().await
}

/// Run PCR prediction using EFI variables and a disk image file.
async fn predict_pcrs(
    efi_vars: &efi::EfiVars,
    disk: &mut fs::File,
    disk_path: &std::path::Path,
    platform: Platform,
    user_data: Option<&str>,
    region: Option<&str>,
) -> Result<PcrPredictions> {
    let gpt_bin = gpt::extract_primary_gpt(disk)?;
    let partitions = gpt::find_partitions(disk)?;
    let shim = diskfs::extract_shim(disk, &partitions)?;
    let grub = diskfs::extract_grub(disk, &partitions)?;
    let vmlinuz = diskfs::extract_vmlinuz(disk, &partitions)?;
    let grub_cfg = diskfs::extract_grub_cfg(disk, &partitions)?;
    let bootconfig = diskfs::extract_bootconfig(disk, &partitions)?;
    let boot_partuuid = gpt::get_boot_partuuid(disk)?;

    let settings_extracted = if user_data.is_some() {
        diskfs::extract_settings_defaults(disk_path, &partitions).ok()
    } else {
        None
    };

    let ctx = PcrContext::builder()
        .platform(platform)
        .efi_vars(efi_vars)
        .partitions(&partitions)
        .gpt_bin(&gpt_bin)
        .shim(&shim)
        .grub(&grub)
        .vmlinuz(&vmlinuz)
        .grub_cfg(&grub_cfg)
        .bootconfig(&bootconfig)
        .boot_partuuid(&boot_partuuid)
        .maybe_user_data(user_data)
        .maybe_settings_defaults(settings_extracted.as_ref().map(|s| s.toml_content.as_str()))
        .maybe_region(region)
        .maybe_variant_id(settings_extracted.as_ref().map(|s| s.variant_id.as_str()))
        .maybe_arch(settings_extracted.as_ref().map(|s| s.arch.as_str()))
        .build();

    PcrPredictions::new()
        .try_extend(|| pcrs::pcr0::predict(&ctx))?
        .try_extend(|| pcrs::pcr1::predict(&ctx))?
        .try_extend(|| pcrs::pcr2::predict(&ctx))?
        .try_extend(|| pcrs::pcr3::predict(&ctx))?
        .try_extend(|| pcrs::pcr4::predict(&ctx))?
        .try_extend(|| pcrs::pcr5::predict(&ctx))?
        .try_extend(|| pcrs::pcr6::predict(&ctx))?
        .try_extend(|| pcrs::pcr7::predict(&ctx))?
        .try_extend_async(pcrs::pcr8::predict(&ctx)).await?
        .try_extend(|| pcrs::pcr9::predict(&ctx))?
        .try_extend(|| pcrs::pcr10::predict(&ctx))?
        .try_extend(|| pcrs::pcr11::predict(&ctx))?
        .try_extend(|| pcrs::pcr12::predict(&ctx))?
        .try_extend(|| pcrs::pcr13::predict(&ctx))?
        .try_extend(|| pcrs::pcr14::predict(&ctx))?
        .try_extend(|| pcrs::pcr15::predict(&ctx))
}
