#!/usr/bin/env python3
"""Check discovery reconfiguration without changing the user's services."""
import re
import runpy
import subprocess
import tempfile
from pathlib import Path
from unittest.mock import patch


def main():
    root = Path(__file__).resolve().parents[1]
    with tempfile.TemporaryDirectory(prefix="gliff-discovery-") as directory:
        test_home = Path(directory)
        server = test_home / "gliff-server"
        server.write_text("#!/bin/sh\n")
        server.chmod(0o700)
        unit = test_home / ".config/systemd/user/gliff-discovery.service"
        active_port = None

        def systemctl(command, **kwargs):
            nonlocal active_port
            assert command[:2] == ["systemctl", "--user"]
            operation = command[2]
            if operation == "restart" or (operation == "enable" and active_port is None):
                active_port = int(re.search(r"--ssh-port (\d+)", unit.read_text())[1])
            return subprocess.CompletedProcess(command, 0)

        for port in (22, 2222, 4422):
            with patch.object(Path, "home", return_value=test_home), \
                 patch("shutil.which", return_value=str(server)), \
                 patch("subprocess.run", side_effect=systemctl), \
                 patch("sys.argv", ["enable-discovery", "--ssh-port", str(port)]):
                runpy.run_path(str(root / "bin/enable-discovery"), run_name="__main__")
            assert active_port == port, f"publisher still advertises {active_port}, expected {port}"
    print("DISCOVERY SERVICE PASS: running publisher follows port changes")


if __name__ == "__main__":
    main()
