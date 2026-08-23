let vhdx_size = if *vhdx_size_mib > 0 {
                *vhdx_size_mib * 1024 * 1024
            } else {
                // Auto: estimate from WIM size + 25% headroom, min 64 GiB
                let iso_mnt = Mount::ro(&image)?;
                let wim = locate_install_wim(iso_mnt.path())?;
                let wim_info = wimlib_info(&wim)?;
                let needed = wim_info.apply_size.unwrap_or(40 * 1024 * 1024 * 1024);
                let vhdx_bytes = ((needed as f64 * 1.25) as u64).max(64 * 1024 * 1024 * 1024);
                iso_mnt.unmount()?;
                vhdx_bytes
            };

            let vhdx_size_sectors = vhdx_size / 512;

            progress_tick(send, "partitioning");
            wipe_signatures(dev)?;
            // Partition layout: ESP (512 MiB) + VHDX partition + optional data
            let mut p_esp = data_part("FERRUS", Some(1_048_576), false); // 512 MiB = 1_048_576 sectors
            p_esp.mbr_type = "0c";
            let mut p_vhdx = data_part("VHDX", Some(vhdx_size / 512), false);
            p_vhdx.mbr_type = "07";
            let mut parts = vec![p_esp, p_vhdx];
            if *persist_mib > 0 {
                let mut p_data = data_part("FERRUSDATA", Some(*persist_mib * 2048), false);
                p_data.mbr_type = "07";
                parts.push(p_data);
            }
            partition(dev, PartitionScheme::Gpt, &parts)?;
            reread_partitions(dev);
            let p_esp_node = part_node(dev, 1);
            let p_vhdx_node = part_node(dev, 2);
            wait_for_node(&p_esp_node, 10)?;
            let p_vhdx_node = part_node(dev, 2);

            progress_tick(send, "creating VHDX image");
            // Create VHDX image in /tmp, apply Windows, then dd to VHDX partition
            let tmp_vhdx = PathBuf::from("/tmp").join(format!("ferrus-{}.vhdx", Uuid::new_v4()));
            std::fs::write(&tmp_vhdx, vec![0u8; vhdx_size as usize])?;
            std::process::Command::new("sync").status()?;

            // Loop-mount the temporary VHDX file, format NTFS inside, apply WIM
            let vhdx_loop = std::process::Command::new("losetup")
                .args(["-f", "--show", tmp_vhdx.to_str().unwrap()])
                .output()?;
            if !vhdx_loop.status.success() {
                bail!("losetup failed: {}", String::from_utf8_lossy(&vhdx_loop.stderr));
            }
            let vhdx_dev = String::from_utf8(vhdx_loop.stdout)?.trim().to_string();
            if vhdx_dev.is_empty() {
                bail!("losetup returned empty device path");
            }
            let vhdx_loop_dev = PathBuf::from(vhdx_dev.trim());
            let _ = std::process::Command::new("udevadm").args(["settle"]).status();
            wait_for_node(&vhdx_loop_dev, 15)?;
            mkfs_any("NTFS", "WINDOWS", &vhdx_loop_dev, None)?;

            progress_tick(send, "applying Windows image into VHDX");
            let iso_mnt = Mount::ro(&image)?;
            let wim = locate_install_wim(iso_mnt.path())?;
            let vhdx_mnt = Mount::rw(&vhdx_loop_dev)?;
            wimlib_apply(
                &wim,
                *wim_index,
                vhdx_mnt.path(),
                cancel,
                pids,
                &mut |d, t, ph| {
                    send(Response::Progress {
                        done: d,
                        total: t,
                        verifying: false,
                        phase: Some(ph.into()),
                    });
                },
            )?;

            if options.any() {
                progress_tick(send, "injecting unattend.xml");
                let panther = vhdx_mnt.path().join("Windows").join("Panther");
                std::fs::create_dir_all(&panther)
                    .context("creating \\Windows\\Panther")?;
                std::fs::write(panther.join("unattend.xml"), ferrus_core::unattend::generate(options))
                    .context("writing Panther unattend.xml")?;
            }

            vhdx_mnt.unmount()?;
            iso_mnt.unmount()?;

            // Detach loop device before copying to partition
            std::process::Command::new("losetup")
                .args(["-d", vhdx_loop_dev.to_str().unwrap()])
                .status()?;

            // Now dd the VHDX file to the VHDX partition
            progress_tick(send, "copying VHDX to partition");
            std::process::Command::new("dd")
                .args(["if", tmp_vhdx.to_str().unwrap(), "of", p_vhdx_node.to_str().unwrap(), "bs=4M", "status=progress"])
                .status()?;
            std::process::Command::new("sync").status()?;
            std::process::Command::new("blockdev").args(["--flushbufs", p_vhdx_node.to_str().unwrap()]).status().ok;
            std::process::Command::new("sync").status()?;

            // Clean up temp VHDX file
            let _ = std::fs::remove_file(&tmp_vhdx);

            // ESP formatting + boot files + BCD with partition device element
            progress_tick(send, "formatting ESP");
            mkfs_any("FAT32", "FERRUS", &p_esp_node, None)?;

            progress_tick(send, "writing boot files");
            let esp = Mount::rw(&p_esp_node)?;
            let boot_dir = esp.path().join("EFI").join("Microsoft").join("Boot");
            std::fs::create_dir_all(&boot_dir)?;
            std::fs::create_dir_all(esp.path().join("EFI").join("Boot"))?;
            let src_fw = iso_mnt
                .path()
                .join("efi")
                .join("microsoft")
                .join("boot")
                .join("bootmgfw.efi");
            std::fs::copy(&src_fw, boot_dir.join("bootmgfw.efi"))
                .with_context(|| format!("copying {}", src_fw.display()))?;
            std::fs::copy(&src_fw, esp.path().join("EFI").join("Boot").join("bootx64.efi"))
                .context("copying fallback bootx64.efi")?;

            let entry_guid = new_entry_guid();
            let disk_guid = gpt_disk_guid(dev_file)?;
            // VHDX partition is partition 2
            let vhdx_ref = ferrus_core::bcd::PartitionRef {
                partition_guid: gpt_part_guid(dev_file, 2)?,
                disk_guid,
            };
            let bcd = ferrus_core::bcd::generate_uefi_bcd_vhdx(
                &entry_guid,
                "Windows To Go",
                &ferrus_core::bcd::PartitionRef {
                    partition_guid: gpt_part_guid(dev_file, 1)?,
                    disk_guid,
                },
                &ferrus_core::bcd::PartitionRef {
                    partition_guid: gpt_part_guid(dev_file, 2)?,
                    disk_guid,
                },
                r"", // No VHD path - we use partition device
                10,
            )?;
            std::fs::write(boot_dir.join("BCD"), &bcd).context("writing BCD")?;

            esp.unmount()?;
            sync_dev(dev);
            Ok(format!(
                "Windows To Go (VHDX partition) ready ({}, entry {entry_guid})",
                scheme.describe()
            ))
        }