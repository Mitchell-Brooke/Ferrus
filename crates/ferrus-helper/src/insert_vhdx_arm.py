import re

with open("/mnt/c/Users/mmjbr/Documents/Ferrus/crates/ferrus-helper/src/ops.rs", "r") as f:
    content = f.read()

idx = content.find("    }\n}\n\n/// Disk GUID")
if idx == -1:
    print("Could not find insertion point")
    exit(1)

first_brace = content.find("}", idx)
second_brace = content.find("}", first_brace + 1)
if second_brace == -1:
    print("Could not find match end brace")
    exit(1)
insert_pos = second_brace

with open("/tmp/new_wtg_vhdx_arm.rs", "r") as f:
    arm_body = f.read()

lines = arm_body.split("\n")
# Find minimum NON-ZERO indent for lines with actual code
min_indent = None
for line in lines:
    stripped = line.strip()
    if stripped:
        indent = len(line) - len(line.lstrip())
        if indent > 0:  # Only consider non-zero indents
            if min_indent is None or indent < min_indent:
                min_indent = indent

if min_indent is None:
    min_indent = 4

print(f"base_indent = {min_indent}")

# Normalize: strip min_indent, add 8 spaces
indented_lines = []
for line in arm_body.split("\n"):
    if line.strip() == "":
        indented_lines.append("")
    else:
        indent = len(line) - len(line.lstrip())
        if indent >= min_indent:
            indented_lines.append("        " + line[min_indent:])
        else:
            indented_lines.append("        " + line.lstrip())

indented_body = "\n".join(indented_lines)

new_arm = """        FlashPlan::WinToGoVhdx {
            wim_index,
            scheme,
            options,
            vhdx_size_mib,
            persist_mib,
            ..
        } => {
""" + indented_body + """
        }"""

with open("/mnt/c/Users/mmjbr/Documents/Ferrus/crates/ferrus-helper/src/ops.rs", "r") as f:
    content = f.read()

idx = content.find("    }\n}\n\n/// Disk GUID")
if idx == -1:
    print("Could not find insertion point")
    exit(1)

first_brace = content.find("}", idx)
second_brace = content.find("}", first_brace + 1)
if second_brace == -1:
    print("Could not find match end brace")
    exit(1)
insert_pos = second_brace

new_content = content[:insert_pos] + "\n" + new_arm + "\n    " + content[insert_pos:]

with open("/mnt/c/Users/mmjbr/Documents/Ferrus/crates/ferrus-helper/src/ops.rs", "w") as f:
    f.write(new_content)
print("INSERTED SUCCESSFULLY")