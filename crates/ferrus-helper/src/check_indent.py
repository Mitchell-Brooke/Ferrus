with open("/mnt/c/Users/mmjbr/Documents/Ferrus/crates/ferrus-helper/src/tmp_new_wtg_vhdx_arm.rs", "r") as f:
    for i, line in enumerate(f):
        if i < 20:
            indent = len(line) - len(line.lstrip())
            print(f"Line {i}: indent={indent}, {repr(line[:60])}")