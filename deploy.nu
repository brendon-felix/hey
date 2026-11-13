#!/usr/bin/env nu

const target = "x86_64-unknown-linux-gnu"
const installed = "/usr/local/bin/hey"

# Builds hey for fermi and restarts its `hey --serve` service on the new binary.
def main [
    --host: string = "felixb@100.112.215.8" # ssh destination; `fermi` resolves to a public port that is closed
] {
    cd $env.FILE_PWD
    let binary = $"target/($target)/release/hey"
    cargo zigbuild --release --target $target

    scp -q $binary $"($host):/tmp/hey"
    # `mv` keeps /tmp's SELinux label, which systemd refuses to execute.
    ssh $host ([
        "sudo systemctl stop hey"
        $"sudo mv /tmp/hey ($installed)"
        $"sudo chown root:root ($installed)"
        $"sudo restorecon ($installed)"
        "sudo systemctl start hey"
    ] | str join " && ")

    let built = open --raw $binary | hash sha256
    let deployed = ssh $host $"sha256sum ($installed)" | split row " " | first
    if $built != $deployed {
        error make { msg: $"fermi runs ($deployed), not the build ($built)" }
    }
    # A binary that fails at startup still starts the service, so give it a moment to exit.
    sleep 2sec
    ssh $host "systemctl is-active --quiet hey && journalctl -u hey -n 1 --no-pager --output cat"
}
