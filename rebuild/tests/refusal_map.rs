//! 旧版退出码构造点的认领索引。
//!
//! `plan/baseline/refusal-map.tsv` 的每一行把一处旧构造点指到新实现与断言它的用例；
//! 本文件按用例把同一簇的编号列成 `covers:` 行，供 `census.py owners` 对账。真正的行为
//! 断言在注释所指的用例里（集成用例在 `tests/`，单元用例在 `src/` 与 `crates/core/src/`）。
//! 末尾一条用例校验两本账自洽：每个旧构造点都有 4 列 map 行，且都在此列出。

// tests/case_runner.rs 的 records_show_refuses_a_damaged_or_foreign_record
// covers: R0001, R0002, R0003, R0137, R0138, R0139, R0140, R0141
// covers: R0142, R0147, R0246, R0247, R0248, R0255, R0256, R0258
// covers: R0259, R0260, R0261, R0263, R0264, R0265, R0266, R0267
// covers: R0269, R0270, R0271, R0273, R0274, R0275, R0276, R0277
// covers: R0459, R0460, R0461, R0462
// tests/gpt_repair.rs 的 a_grown_container_is_repaired_in_the_same_table_write
// covers: R0005, R0006, R0007, R0008, R0009, R0010, R0011
// covers: R0602, R0603, R0604, R0605, R0606, R0607, R0608
// tests/case_runner.rs 的 check_reports_the_reason_it_cannot_act
// covers: R0104, R0105, R0106, R0107
// tests/case_runner.rs 的 resize_rewrites_the_table_and_refuses_unsafe_shrinks
// covers: R0012, R0013, R0015, R0027, R0190, R0191, R0200, R0201
// covers: R0202, R0204, R0205, R0206, R0207, R0208, R0210, R0211
// covers: R0327, R0328, R0330, R0331, R0332, R0333, R0334, R0335
// covers: R0336, R0337, R0338, R0339, R0340, R0343, R0344, R0345
// covers: R0350, R0351, R0357, R0361, R0363, R0368, R0369, R0370
// covers: R0371, R0377, R0378, R0379, R0380, R0381, R0382, R0384
// tests/case_runner.rs 的 filesystem_commands_refuse_without_touching_the_target
// covers: R0016, R0017, R0095, R0099, R0100, R0101, R0102, R0103
// covers: R0532
// tests/case_runner.rs 的 add_delete_and_set_rewrite_both_copies
// covers: R0019, R0020, R0028, R0111, R0117, R0118, R0122, R0123
// covers: R0126, R0129, R0169, R0175, R0179, R0189, R0585
// crates/core 的 guid_parse_rejects_wrong_shapes
// covers: R0022
// tests/case_runner.rs 的 move_and_copy_relocate_data_and_keep_type_guids
// covers: R0025, R0030, R0031, R0032, R0034, R0035, R0037, R0038
// covers: R0039, R0040, R0243, R0244, R0245, R0533, R0534, R0535
// covers: R0656, R0661, R0663, R0664
// tests/refusal_claims.rs 的 an_unusable_identity_cannot_plan_a_write
// covers: R0041, R0355, R0356, R0793, R0810, R0811
// tests/fault_injection.rs 的 a_table_that_changes_after_the_write_reports_partial
// covers: R0042, R0043, R0217, R0517, R0588, R0589, R0590, R0592
// covers: R0858, R0928, R0931, R0932
// tests/cases/mod.rs 的 records show without a target
// covers: R0045
// tests/cases/mod.rs 的 unknown command is a usage error
// covers: R0047, R0070, R0073, R0079, R0080, R0093
// tests/case_runner.rs 的 resize_refuses_a_target_without_a_table_or_a_missing_partition
// covers: R0048
// tests/cases/mod.rs 的 records list refuses a partition selector
// covers: R0051, R0084, R0087
// tests/cases/mod.rs 的 records verify without an id
// covers: R0053
// tests/cases/mod.rs 的 records list --root without a value is a usage error
// covers: R0056
// tests/cases/mod.rs 的 resize without a size is a usage error
// covers: R0058
// tests/cases/mod.rs 的 set type refuses an empty value
// covers: R0062
// tests/cases/mod.rs 的 resize --fs-only without a target is a usage error
// covers: R0065, R0066
// tests/cases/mod.rs 的 add refuses an MBR type byte for GPT
// covers: R0068
// tests/cases/mod.rs 的 unknown help topic is a usage error
// covers: R0090
// tests/online.rs 的 online_grow_succeeds_only_after_the_read_back_reaches_the_partition
// covers: R0097, R0326, R0421, R0422, R0423, R0424, R0425, R0426
// covers: R0427, R0428, R0429, R0430, R0431, R0432, R0433, R0434
// covers: R0733, R0734, R0735, R0736, R0737, R0738, R0739, R0740
// covers: R0741, R0742, R0743, R0744, R0745, R0746, R0747, R0748
// covers: R0749, R0750, R0751, R0752, R0753, R0754, R0755, R0756
// tests/fault_injection.rs 的 a_host_without_a_backend_refuses_with_the_unsupported_code
// covers: R0144, R0145, R0146
// tests/case_runner.rs 的 new_writes_a_readable_table_and_refuses_to_clobber
// covers: R0156, R0159, R0160
// tests/fault_injection.rs 的 the_table_preimage_is_recorded_before_the_table_write
// covers: R0161, R0162, R0163, R0164, R0391, R0392, R0463, R0518
// covers: R0801, R0802, R0803, R0804, R0806, R0807, R0809, R0860
// covers: R0929, R0930, R0933, R0934
// tests/refusal_claims.rs 的 mbr_add_uses_the_first_empty_slot_and_rejects_overlap
// covers: R0181, R0906
// tests/refusal_claims.rs 的 mbr_delete_rejects_empty_structural_and_out_of_range_slots
// covers: R0186
// tests/refusal_claims.rs 的 relocation_layout_rejects_zero_geometry_and_unaddressable_shifts
// covers: R0193, R0668, R0669, R0670, R0672, R0673, R0675, R0676
// covers: R0677
// tests/fault_injection.rs 的 a_failed_progress_checkpoint_reports_partial
// covers: R0194, R0195, R0196, R0197, R0198, R0199, R0215, R0514
// covers: R0852
// tests/fault_injection.rs 的 a_failed_source_read_changes_nothing_and_reports_no_mutation
// covers: R0212, R0213, R0214, R0323, R0324, R0325, R0710, R0711
// covers: R0712, R0713, R0714, R0715, R0716, R0718, R0719, R0720
// tests/fault_injection.rs 的 a_failed_table_write_after_the_move_reports_partial
// covers: R0216, R0388, R0389, R0390, R0515, R0516, R0854, R0856
// covers: R0964, R0965, R0966
// tests/cases/mod.rs 的 add refuses --start align
// covers: R0218, R0219
// tests/cases/mod.rs 的 move without --start is a usage error
// covers: R0220
// tests/fault_injection.rs 的 a_write_failure_after_the_first_block_reports_partial
// covers: R0224, R0225, R0226, R0229, R0230, R0231, R0232, R0233
// covers: R0234, R0235, R0237, R0238, R0239, R0240, R0241, R0519
// covers: R0520, R0521, R0522, R0523, R0524, R0525, R0526, R0527
// covers: R0528, R0529, R0722, R0723, R0724, R0725, R0726, R0727
// covers: R0728, R0729
// tests/contract.rs 的 command_surface_has_sixteen_named_entries
// covers: R0249, R0250, R0251, R0252, R0253, R0254
// tests/cases/mod.rs 的 records list --root needs a directory
// covers: R0278, R0279, R0280, R0281, R0283, R0284, R0285, R0288
// covers: R0289
// tests/case_runner.rs 的 undo_refuses_a_target_without_a_record
// covers: R0290, R0291, R0292, R0293, R0294, R0295, R0296, R0297
// covers: R0298, R0299, R0300, R0815
// tests/refusal_claims.rs 的 release_archives_instead_of_touching_the_target
// covers: R0303, R0304, R0305, R0306, R0307, R0308, R0309, R0310
// covers: R0311
// tests/refusal_claims.rs 的 a_second_release_does_not_overwrite_the_first_archive
// covers: R0312, R0313, R0314, R0315, R0316, R0317, R0318, R0319
// covers: R0320, R0321, R0322
// tests/case_runner.rs 的 superfloppy_resize_grows_the_filesystem_and_refuses_unextendable_requests
// covers: R0352, R0353, R0354, R0451
// covers: R0761, R0762, R0763, R0764, R0765, R0766
// covers: R0767, R0768, R0769, R0770, R0771, R0772, R0773, R0774
// covers: R0775, R0776, R0777, R0778, R0779, R0780
// tests/refusal_claims.rs 的 mbr_resize_rejects_structural_and_out_of_bounds_ranges
// covers: R0393, R0419, R0420, R0893
// tests/case_runner.rs 的 resize_refuses_mbr_relocation_flags_and_no_table_targets
// covers: R0400, R0405, R0406, R0407, R0408, R0409, R0410, R0412
// covers: R0415, R0416
// tests/refusal_claims.rs 的 a_repaired_filesystem_is_a_success_and_a_later_failure_is_partial
// covers: R0435, R0436, R0437, R0530, R0531
// tests/online.rs 的 a_sector_size_on_a_block_device_is_refused_before_it_is_opened
// covers: R0453, R0454, R0455, R0456, R0594, R0595, R0596, R0597
// covers: R0598, R0599, R0794, R0795, R0797, R0967, R0968, R0969
// covers: R0970, R0971, R0972, R0973, R0974
// crates/core 的 a_multi_device_btrfs_member_is_occupied
// covers: R0457, R0458, R0721
// tests/fault_injection.rs 的 a_relocation_interruption_leaves_only_table_preimages_and_keeps_old_source
// covers: R0464, R0465, R0466, R0467, R0468, R0469, R0470, R0471
// covers: R0472, R0473, R0474, R0475, R0476, R0555, R0556, R0557
// covers: R0678, R0683, R0688, R0689, R0690, R0691, R0692, R0693
// covers: R0694, R0695, R0696, R0697, R0698, R0699, R0700, R0701
// covers: R0702, R0703, R0704, R0705, R0706, R0707, R0708, R0791
// tests/carrier.rs 的 the_carrier_grows_a_mounted_filesystem_without_the_host_capability
// covers: R0477, R0478, R0479, R0480, R0486, R0487, R0488, R0489
// covers: R0501, R0502, R0503, R0513, R0757, R0758, R0759, R0760
// tests/fault_injection.rs 的 a_clean_run_commits_and_cleans_the_record
// covers: R0481, R0482, R0536, R0537, R0538, R0539, R0540, R0541
// covers: R0543, R0544, R0545, R0546, R0547, R0548, R0549, R0550
// covers: R0551, R0552, R0553, R0554, R0812, R0813, R0814
// crates/core 的 every_program_identity_is_registered
// covers: R0483, R0484, R0485
// crates/core 的 a_matching_bundle_is_accepted
// covers: R0490, R0491, R0492, R0493, R0494
// crates/core 的 a_path_leaving_the_bundle_is_refused
// covers: R0495, R0496, R0497, R0498, R0499
// crates/core 的 a_digest_mismatch_is_refused_before_boot
// covers: R0500
// crates/core 的 a_foreign_record_is_refused_and_never_rolled_back
// covers: R0504, R0505, R0506, R0507, R0508, R0509, R0510, R0511
// crates/core 的 a_malformed_worker_digest_or_a_different_version_is_refused
// covers: R0512
// tests/fault_injection.rs 的 a_failed_data_flush_reports_partial
// covers: R0558, R0559, R0560, R0561, R0562, R0563, R0564, R0565
// covers: R0566, R0567, R0568, R0569, R0570, R0571, R0572, R0573
// covers: R0574, R0575, R0576
// crates/core 的 swap_is_rebuilt_rather_than_resized_in_place
// covers: R0587, R0657
// tests/refusal_claims.rs 的 alignment_overflow_rejects_the_start
// covers: R0600, R0601, R0862
// crates/core 的 classify_falls_back_to_backup
// covers: R0609, R0610
// crates/core 的 missing_signature_is_absent_not_damaged
// covers: R0612, R0614
// tests/refusal_claims.rs 的 gpt_placement_rejects_address_space_overflow
// covers: R0620, R0621
// crates/core 的 usable_range_rejects_inverted_bounds
// covers: R0624
// tests/refusal_claims.rs 的 gpt_add_slot_rejects_bounds_overlap_slots_and_full_table
// covers: R0627, R0634, R0635, R0636
// tests/refusal_claims.rs 的 gpt_flag_names_reject_unknown_values
// covers: R0638, R0640, R0641, R0644
// tests/refusal_claims.rs 的 partition_lookup_rejects_empty_and_missing_entries
// covers: R0646, R0647, R0648, R0649
// tests/contract.rs 的 help_and_usage_choose_the_documented_process_codes
// covers: R0650, R0651, R0652
// crates/core 的 preserved_type_guids_rejects_a_rewritten_type
// covers: R0658, R0659
// tests/case_runner.rs 的 resize_allow_move_relocates_the_blocking_partition
// covers: R0667
// tests/refusal_claims.rs 的 a_record_for_another_target_is_not_resumed
// covers: R0684, R0685, R0686, R0687, R0798, R0799, R0800, R0825
// covers: R0826, R0840
// tests/fault_injection.rs 的 data_that_does_not_read_back_reports_partial
// covers: R0731, R0732
// tests/refusal_claims.rs 的 recovery_outcomes_are_not_success
// covers: R0781
// tests/contract.rs 的 outcomes_and_refusals_map_to_the_seven_codes
// covers: R0786, R0787, R0788
// tests/refusal_claims.rs 的 a_foreign_record_is_refused_and_never_rolled_back
// covers: R0789, R0790, R0822, R0823
// crates/core 的 truncation_at_any_byte_has_a_deterministic_plan
// covers: R0816, R0817, R0818
// crates/core 的 prefix_ending_in_a_completed_frame_is_replayed
// covers: R0819, R0820, R0821
// crates/core 的 rollback_verifies_the_preimage_and_cleans_the_record
// covers: R0824, R0827, R0828, R0829, R0830, R0831
// crates/core 的 a_recorded_range_past_the_target_is_refused
// covers: R0832, R0833, R0834, R0835
// crates/core 的 b21_resume_checks_prefix_digest
// covers: R0836, R0837
// crates/core 的 equal_sets_have_no_difference
// covers: R0838, R0839
// crates/core 的 disjoint_sets_are_fully_added_and_removed
// covers: R0841, R0842, R0843, R0844
// crates/core 的 prefix_ending_in_an_incomplete_frame_rolls_back
// covers: R0845, R0846, R0847, R0848, R0849, R0850
// crates/core 的 byte_conversions_cover_extremes
// covers: R0866, R0867
// crates/core 的 protective_mbr_is_rewritten_only_when_non_canonical
// covers: R0878, R0879, R0880, R0956
// crates/core 的 mbr_creation_preserves_boot_code_and_clears_entries
// covers: R0885, R0941, R0957
// tests/refusal_claims.rs 的 mbr_parse_rejects_unsigned_and_short_sectors
// covers: R0888
// tests/fault_injection.rs 的 a_failed_table_flush_reports_partial
// covers: R0935
// crates/core 的 gpt_content_writes_both_copies_with_consistent_crc
// covers: R0936, R0937, R0938, R0939, R0940, R0943, R0944, R0945
// covers: R0946, R0947, R0948, R0949, R0950, R0951, R0952, R0953
// covers: R0954, R0955
// crates/core 的 table_capacity_refuses_disks_too_small_for_gpt_or_mbr
// covers: R0942, R0959, R0960, R0961, R0962, R0963

#[test]
fn every_legacy_refusal_site_has_a_four_column_row_and_a_covers_entry() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let sites =
        std::fs::read_to_string(root.join("plan/baseline/refusal-sites.tsv")).expect("sites");
    let map = std::fs::read_to_string(root.join("plan/baseline/refusal-map.tsv")).expect("map");
    // covers 行可以落在任一个 tests 下的 .rs 里，递归拼接后统一核对。
    let mut cov = String::new();
    let mut stack = vec![root.join("tests")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("tests dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                cov.push_str(&std::fs::read_to_string(&path).expect("test source"));
                cov.push('\n');
            }
        }
    }
    let ids: Vec<&str> = sites
        .lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split('\t').next().unwrap())
        .collect();
    assert!(!ids.is_empty(), "no legacy sites listed");
    let rows: Vec<&str> = map
        .lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .collect();
    for id in &ids {
        let row = rows
            .iter()
            .find(|r| r.split('\t').next() == Some(id))
            .unwrap_or_else(|| panic!("{id} has no map row"));
        let cols: Vec<&str> = row.split('\t').collect();
        assert!(
            cols.len() >= 4 && cols[1..4].iter().all(|c| !c.trim().is_empty()),
            "{id} row is incomplete"
        );
        assert!(cov.contains(id), "{id} is not listed in a covers line");
    }
}
