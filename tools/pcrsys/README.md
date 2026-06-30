# pcrsys

A TPM Platform Configuration Register (PCR) prediction tool for Bottlerocket.

## Overview

`pcrsys` predicts TPM PCR values (SHA-256) for Bottlerocket images without requiring a running system. It analyzes disk images and EFI variables to compute what the PCR values would be after boot, enabling pre-registration of expected measurements for attestation policies.

## Usage

### From a local disk image

```bash
pcrsys disk --image /path/to/disk.img --efi-vars /path/to/efi-vars.json [--platform aws|vmware|metal]
```

### From an AWS AMI

```bash
pcrsys ami --ami-id ami-0123456789abcdef0 [--region us-west-2] [--profile myprofile]
```

The AMI subcommand downloads the root snapshot using coldsnap and retrieves UEFI data from the AMI attributes.

### Predicting PCR 8 (user-data / settings)

Bottlerocket measures merged node settings into PCR 8. Predicting this value requires:

1. **User-data TOML** (`--user-data`) — the settings that will be applied to the instance at boot.
2. **Settings defaults TOML** (`--settings-defaults`) — the image's baked-in defaults. If not provided, pcrsys attempts to extract defaults from ROOT-A on the disk image. Images using erofs for ROOT-A require this flag.

The defaults TOML is the merged output of a variant's `defaults.d/` directory (e.g., from `bottlerocket-core-kit/sources/settings-defaults/aws-k8s-1.31/defaults.d/`). It corresponds to the file installed at `/usr/share/storewolf/<variant>.toml` on the running system.

```bash
# PCR 8 prediction with explicit settings defaults (recommended for erofs images)
pcrsys ami --ami-id ami-0123456789abcdef0 \
  --user-data /path/to/user-data.toml \
  --settings-defaults /path/to/aws-k8s-1.31.toml

# PCR 8 prediction from local disk (auto-extracts defaults from ext4 ROOT-A)
pcrsys disk --image /path/to/disk.img --efi-vars /path/to/efi-vars.json \
  --user-data /path/to/user-data.toml
```

## Output

JSON output with predicted PCR values, keyed by PCR index:

```json
{
  "pcrs": {
    "0": { "sha256": ["<hex digest>"] },
    "4": { "sha256": ["<hex digest>"] },
    "7": { "sha256": ["<hex digest>"] }
  }
}
```

## Supported PCRs

| PCR | Description |
|-----|-------------|
| 0 | Platform firmware (static per platform) |
| 1 | Platform configuration (static per platform) |
| 2 | Option ROM code (separator only) |
| 3 | Option ROM configuration (separator only) |
| 4 | Boot manager code (shim, grub, vmlinuz authenticode hashes) |
| 5 | GPT partition table |
| 6 | Resume events (separator only) |
| 7 | Secure Boot policy (PK, KEK, db, dbx, SbatLevel, MokListRT) |
| 8 | Settings measurement (requires `--user-data`; merged defaults + user TOML, canonical JSON) |
| 9 | Kernel command line (grub.cfg + bootconfig) |
| 10 | Zero (unused) |
| 11 | Boot phases (systemd) |
| 12 | Zero (unused) |
| 13 | Zero (unused) |
| 14 | Shim MOK (MokList, MokListX, MokListTrusted) |
| 15 | Zero (unused) |

PCRs 4 and 9 are skipped for images with A/B boot partitions since the active kernel and root hash can change.

## Supported Platforms

- **aws**: AWS Nitro (EC2 instances)
- **vmware**: VMware vSphere
- **metal**: Bare metal servers

Platform differences affect PCR 0, 1, 4, 5, and 7 calculations due to firmware behavior variations.

## Input Requirements

### efi-vars.json

JSON file containing Secure Boot variables:

```json
{
  "variables": [
    { "name": "PK", "guid": "8be4df61-93ca-11d2-aa0d-00e098032b8c", "data": "<hex>" },
    { "name": "KEK", "guid": "8be4df61-93ca-11d2-aa0d-00e098032b8c", "data": "<hex>" },
    { "name": "db", "guid": "d719b2cb-3d3a-4596-a3bc-dad00e67656f", "data": "<hex>" },
    { "name": "dbx", "guid": "d719b2cb-3d3a-4596-a3bc-dad00e67656f", "data": "<hex>" }
  ]
}
```

### Disk image

GPT-partitioned disk image containing:
- EFI System Partition (FAT) with `/EFI/BOOT/boot{aa64,x64}.efi` (shim) and `grub{aa64,x64}.efi`
- Boot partition (ext4) with `/vmlinuz`, `/grub.cfg`, and `/bootconfig.data`
