#!/usr/bin/env python3
"""Check the GUI subsystem and embedded Roam icon in a built Windows executable."""

import argparse
from pathlib import Path
import struct


def check_binary(executable, icon):
    data = Path(executable).read_bytes()

    def read(fmt, offset):
        return struct.unpack_from("<" + fmt, data, offset)

    if data[:2] != b"MZ":
        raise ValueError("not a Windows executable")
    pe = read("I", 60)[0]
    if data[pe:pe + 4] != b"PE\0\0":
        raise ValueError("missing PE header")
    sections = read("H", pe + 6)[0]
    optional_size = read("H", pe + 20)[0]
    optional = pe + 24
    magic = read("H", optional)[0]
    if magic not in (0x10B, 0x20B):
        raise ValueError("unsupported PE optional header")
    if read("H", optional + 68)[0] != 2:
        raise ValueError("roam.exe must use the Windows GUI subsystem, not the console subsystem")

    directories = optional + (112 if magic == 0x20B else 96)
    resource_rva, resource_size = read("II", directories + 16)
    if not resource_rva or not resource_size:
        raise ValueError("roam.exe has no embedded resources")
    section_table = optional + optional_size

    def file_offset(rva, size):
        for index in range(sections):
            _, virtual, raw_size, raw = read("IIII", section_table + index * 40 + 8)
            if virtual <= rva and rva + size <= virtual + raw_size:
                return raw + rva - virtual
        raise ValueError("resource RVA is outside the file's sections")

    resource_base = file_offset(resource_rva, resource_size)

    def entries(relative):
        directory = resource_base + relative
        named, numbered = read("HH", directory + 12)
        return [read("II", directory + 16 + index * 8) for index in range(named + numbered)]

    def child(directory, identifier):
        for name, target in entries(directory):
            if name == identifier and target & 0x80000000:
                return target & 0x7FFFFFFF
        raise ValueError(f"missing resource directory {identifier}")

    def resource(kind, identifier):
        languages = entries(child(child(0, kind), identifier))
        for _, target in languages:
            if not target & 0x80000000:
                rva, size, _, _ = read("IIII", resource_base + target)
                offset = file_offset(rva, size)
                return data[offset:offset + size]
        raise ValueError("missing resource data")

    # GPUI uses LoadImageW(module, MAKEINTRESOURCE(1), IMAGE_ICON, ...).
    group = resource(14, 1)  # RT_GROUP_ICON
    reserved, kind, count = struct.unpack_from("<HHH", group)
    original = Path(icon).read_bytes()
    source_header = struct.unpack_from("<HHH", original)
    if (reserved, kind, count) != source_header or reserved != 0 or kind != 1 or count == 0:
        raise ValueError("embedded icon group does not match roam.ico")
    for index in range(count):
        image = struct.unpack_from("<BBBBHHIH", group, 6 + index * 14)
        source = struct.unpack_from("<BBBBHHII", original, 6 + index * 16)
        # Resource compilers normalize fields such as planes (0 -> 1).
        if image[:2] != source[:2] or image[6] != source[6]:
            raise ValueError("embedded icon dimensions do not match roam.ico")
        expected = original[source[7]:source[7] + source[6]]
        if resource(3, image[7]) != expected:  # RT_ICON
            raise ValueError("embedded icon pixels do not match roam.ico")
    return count


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("executable", type=Path)
    parser.add_argument("--icon", type=Path, default=Path(__file__).resolve().parent.parent / "assets/app-icon/roam.ico")
    args = parser.parse_args()
    try:
        count = check_binary(args.executable, args.icon)
    except (OSError, ValueError, struct.error) as error:
        parser.exit(1, f"Windows binary check failed: {error}\n")
    print(f"Verified Windows GUI subsystem and {count} embedded Roam icon sizes: {args.executable}")


if __name__ == "__main__":
    main()
