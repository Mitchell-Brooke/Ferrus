with open("/mnt/c/Users/mmjbr/Documents/Ferrus/crates/ferrus-helper/src/ops.rs", "r") as f:
    content = f.read()

search_text = "    }\n}\n\n/// Disk GUID"
idx = content.find(search_text)
if idx == -1:
    print("NOT FOUND with exact text")
    idx = content.find("/// Disk GUID")
    if idx != -1:
        print("Found gpt header at:", idx)
        print("Context:", repr(content[idx-60:idx+50]))
else:
    print("Found at:", idx)
    print("Context:", repr(content[idx-50:idx+60]))