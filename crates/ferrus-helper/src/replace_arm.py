import re

with open("/mnt/c/Users/mmjbr/Documents/Ferrus/crates/ferrus-helper/src/ops.rs", "r") as f:
    content = f.read()

start = content.find("FlashPlan::WinToGoVhdx")
if start == -1:
    print("NOT FOUND")
    exit(1)

brace_count = 0
i = start
while i < len(content):
    if content[i] == "{":
        brace_count += 1
    elif content[i] == "}":
        brace_count -= 1
        if brace_count == 0:
            end = i + 1
            break
    i += 1
else:
    print("Could not find matching brace")
    exit(1)

print("Found arm from {} to {}".format(start, end))
print("Length: {}".format(end - start))

with open("/tmp/new_wtg_vhdx_arm.rs", "r") as f:
    new_arm = f.read()

new_content = content[:start] + new_arm + content[end:]
with open("/mnt/c/Users/mmjbr/Documents/Ferrus/crates/ferrus-helper/src/ops.rs", "w") as f:
    f.write(new_content)
print("REPLACED SUCCESSFULLY")