"""Fail if a packaged Mach-O file still links outside the app or system libraries."""
from pathlib import Path
import subprocess
import sys

app = Path(sys.argv[1]).resolve()
executable_dir = app / "Contents/MacOS"
files = [executable_dir / "openxcloud", *sorted((app / "Contents/Frameworks").glob("*.dylib"))]
for file in files:
    output = subprocess.check_output(["otool", "-L", str(file)], text=True)
    for line in output.splitlines()[1:]:
        dependency = line.strip().split(" (", 1)[0]
        if dependency.startswith(("/System/Library/", "/usr/lib/")):
            continue
        resolved = dependency.replace("@executable_path", str(executable_dir)).replace("@loader_path", str(file.parent))
        path = Path(resolved).resolve()
        if not path.is_relative_to(app) or not path.is_file():
            raise SystemExit(f"Unbundled dependency in {file.name}: {dependency}")
print(f"Validated dependencies for {len(files)} Mach-O files")
