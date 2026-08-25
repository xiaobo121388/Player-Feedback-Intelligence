"""Deploy the standalone Rust web service without persisting deployment secrets."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import sys
import tarfile
import tempfile
import time

import paramiko


ROOT = Path(__file__).resolve().parents[2]
USER = os.environ.get("MC_DEPLOY_USER", "root")
REMOTE_ARCHIVE = "/tmp/mc-feedback-web-source.tar.gz"
REMOTE_BUILD = "/tmp/mc-feedback-web-build"
REMOTE_TARGET = "/tmp/mc-feedback-web-target"


def required_environment(name: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        raise RuntimeError(f"missing environment variable: {name}")
    return value


def web_domain() -> str:
    value = required_environment("MC_WEB_DOMAIN").lower()
    if not re.fullmatch(r"[a-z0-9](?:[a-z0-9.-]{0,251}[a-z0-9])?", value):
        raise RuntimeError("MC_WEB_DOMAIN is not a valid DNS name")
    return value


def public_url() -> str:
    return "https://" + web_domain()


def desktop_helper() -> Path:
    path = Path(required_environment("MC_DESKTOP_HELPER")).expanduser().resolve()
    if not path.is_file():
        raise FileNotFoundError(f"desktop helper not found: {path}")
    return path


def connect() -> paramiko.SSHClient:
    password = os.environ.get("MC_DEPLOY_PASSWORD") or None
    client = paramiko.SSHClient()
    client.load_system_host_keys()
    client.set_missing_host_key_policy(paramiko.RejectPolicy())
    client.connect(
        required_environment("MC_DEPLOY_HOST"),
        username=USER,
        password=password,
        look_for_keys=password is None,
        allow_agent=password is None,
        timeout=20,
    )
    return client


def run(
    client: paramiko.SSHClient,
    command: str,
    *,
    stdin: str | bytes | None = None,
    timeout: int = 600,
    show: bool = True,
) -> str:
    channel = client.get_transport().open_session(timeout=20)
    channel.set_combine_stderr(True)
    channel.exec_command(command)
    if stdin is not None:
        payload = stdin.encode("utf-8") if isinstance(stdin, str) else stdin
        channel.sendall(payload)
    channel.shutdown_write()
    channel.settimeout(1)
    output = bytearray()
    deadline = time.monotonic() + timeout
    while not channel.exit_status_ready() or channel.recv_ready():
        if time.monotonic() > deadline:
            channel.close()
            raise TimeoutError("remote command timed out")
        if channel.recv_ready():
            chunk = channel.recv(32768)
            output.extend(chunk)
            if show:
                sys.stdout.write(chunk.decode("utf-8", errors="replace"))
                sys.stdout.flush()
        else:
            time.sleep(0.1)
    code = channel.recv_exit_status()
    text = output.decode("utf-8", errors="replace")
    if code:
        raise RuntimeError(f"remote command failed with exit code {code}")
    return text


def source_archive() -> Path:
    handle = tempfile.NamedTemporaryFile(prefix="mc-feedback-web-", suffix=".tar.gz", delete=False)
    handle.close()
    archive = Path(handle.name)
    roots = [
        ROOT / "Cargo.toml",
        ROOT / "Cargo.lock",
        ROOT / "web-server",
        ROOT / "src-tauri",
    ]

    def allowed(path: Path) -> bool:
        return not any(part in {"target", "__pycache__", ".test-web"} for part in path.parts)

    with tarfile.open(archive, "w:gz") as bundle:
        for item in roots:
            if item.is_file():
                bundle.add(item, arcname=item.relative_to(ROOT).as_posix())
                continue
            for path in item.rglob("*"):
                if path.is_file() and allowed(path):
                    bundle.add(path, arcname=path.relative_to(ROOT).as_posix())
    return archive


def upload(client: paramiko.SSHClient, local: Path, remote: str) -> None:
    sftp = client.open_sftp()
    try:
        sftp.put(str(local), remote)
    finally:
        sftp.close()


def upload_nginx_config(client: paramiko.SSHClient) -> None:
    template = (ROOT / "deploy/web/nginx-mc-feedback.conf").read_text(encoding="utf-8")
    rendered = template.replace("__MC_WEB_DOMAIN__", web_domain())
    handle = tempfile.NamedTemporaryFile(
        prefix="mc-feedback-nginx-",
        suffix=".conf",
        mode="w",
        encoding="utf-8",
        delete=False,
    )
    try:
        with handle:
            handle.write(rendered)
        upload(client, Path(handle.name), "/tmp/nginx-mc-feedback.conf")
    finally:
        Path(handle.name).unlink(missing_ok=True)


def inspect(client: paramiko.SSHClient) -> None:
    command = r"""
set -eu
echo "OS=$(uname -srmo)"
echo "RUST=$(rustc --version 2>/dev/null || echo missing)"
echo "MEMORY=$(free -m | awk '/^Mem:/{print $2 "MB total, " $7 "MB available"}')"
echo "SWAP=$(free -m | awk '/^Swap:/{print $2 "MB"}')"
echo "DISK=$(df -h / | awk 'NR==2{print $4 " available"}')"
for unit in feedback-n8n mc-feedback-gateway mc-feedback-web mc-feedback-web-staging; do
  echo "$unit=$(systemctl is-active "$unit" 2>/dev/null || true)"
done
echo "PORTS=$(ss -ltnp | awk '$4 ~ /:(5678|15678|18080)$/ {print $4}' | paste -sd, -)"
if [ -x /opt/mc-feedback-web/mc-feedback-web ]; then
  echo "WEB_BINARY=$(du -h /opt/mc-feedback-web/mc-feedback-web | awk '{print $1}')"
fi
if systemctl is-active --quiet mc-feedback-web-staging 2>/dev/null; then
  echo "WEB_STAGE_MEMORY=$(systemctl show mc-feedback-web-staging -p MemoryCurrent --value)"
fi
if systemctl is-active --quiet mc-feedback-web 2>/dev/null; then
  echo "WEB_MEMORY=$(systemctl show mc-feedback-web -p MemoryCurrent --value)"
fi
if command -v python3 >/dev/null 2>&1; then
  n8n_db=$(find /var/lib/feedback-ai -maxdepth 4 -type f -name database.sqlite -print -quit 2>/dev/null || true)
  if [ -n "$n8n_db" ]; then
    echo "N8N_EMAIL=$(N8N_DB="$n8n_db" python3 -c 'import os,sqlite3; print(sqlite3.connect(os.environ["N8N_DB"]).execute("select email from user limit 1").fetchone()[0])' 2>/dev/null || true)"
  fi
fi
dbus-run-session -- sh -c '
  printf "\n" | gnome-keyring-daemon --unlock --components=secrets >/dev/null 2>&1 || true
  if secret-tool lookup service com.mcfeedback.viewer username netease-session >/dev/null 2>&1; then
    echo NETEASE_SESSION=present
  else
    echo NETEASE_SESSION=not-found
  fi
' 2>/dev/null || echo NETEASE_SESSION=check-failed
if [ -f /etc/systemd/system/mc-feedback-web-staging.service ] && ! systemctl is-active --quiet mc-feedback-web-staging 2>/dev/null; then
  journalctl -u mc-feedback-web-staging -n 20 --no-pager 2>/dev/null || true
fi
"""
    run(client, "bash -se", stdin=command)


def ensure_build_tools(client: paramiko.SSHClient) -> None:
    script = r"""
set -eux
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends build-essential ca-certificates cmake curl pkg-config
if [ ! -x /root/.cargo/bin/rustc ] || ! /root/.cargo/bin/rustc --version | awk '{split($2,v,"."); exit !(v[1]>1 || (v[1]==1 && v[2]>=88))}'; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --profile minimal --default-toolchain stable
fi
/root/.cargo/bin/rustc --version
"""
    run(client, "bash -se", stdin=script, timeout=900)


def migrate_session(client: paramiko.SSHClient) -> None:
    script = r"""
set -eu
dbus-run-session -- sh -eu -c '
  printf "\n" | gnome-keyring-daemon --unlock --components=secrets >/dev/null 2>&1 || true
  token=$(secret-tool lookup service com.mcfeedback.viewer username netease-session 2>/dev/null || true)
  if [ -z "$token" ]; then
    echo "无法从原 stdio MCP 钥匙串读取网易会话" >&2
    exit 4
  fi
  printf %s "$token" |
    sudo -u feedback-ai sh -c "set -a; . /etc/mc-feedback-web/web.env; set +a; exec /opt/mc-feedback-web/mc-feedback-web --import-session"
'
"""
    run(client, "bash -se", stdin=script, show=False)
    print("NETEASE_SESSION=migrated")


def bootstrap_payload() -> str:
    names = [
        "MC_WEB_ADMIN_EMAIL",
        "MC_WEB_ADMIN_PASSWORD",
        "MC_WEB_MODEL_BASE_URL",
        "MC_WEB_MODEL",
        "MC_WEB_MODEL_API_KEY",
    ]
    missing = [name for name in names if not os.environ.get(name)]
    if missing:
        raise RuntimeError("missing environment variables: " + ", ".join(missing))
    return json.dumps(
        {
            "admin_email": os.environ["MC_WEB_ADMIN_EMAIL"],
            "admin_password": os.environ["MC_WEB_ADMIN_PASSWORD"],
            "model_base_url": os.environ["MC_WEB_MODEL_BASE_URL"],
            "model": os.environ["MC_WEB_MODEL"],
            "model_api_key": os.environ["MC_WEB_MODEL_API_KEY"],
            "legacy_netease_account": os.environ.get("MC_WEB_LEGACY_NETEASE_ACCOUNT"),
        },
        ensure_ascii=False,
    )


def stage(client: paramiko.SSHClient) -> None:
    helper = desktop_helper()
    archive = source_archive()
    try:
        upload(client, archive, REMOTE_ARCHIVE)
    finally:
        archive.unlink(missing_ok=True)
    upload(client, ROOT / "deploy/web/mc-feedback-web.service", "/tmp/mc-feedback-web.service")
    upload(client, ROOT / "deploy/web/web.env", "/tmp/mc-feedback-web.env")
    upload(client, helper, "/tmp/mc-feedback-viewer-windows-x64.exe")
    ensure_build_tools(client)
    build = rf"""
set -eux
if [ -d '{REMOTE_BUILD}/target' ] && [ ! -d '{REMOTE_TARGET}' ]; then
  mv '{REMOTE_BUILD}/target' '{REMOTE_TARGET}'
fi
rm -rf '{REMOTE_BUILD}'
mkdir -p '{REMOTE_BUILD}'
tar -xzf '{REMOTE_ARCHIVE}' -C '{REMOTE_BUILD}'
cd '{REMOTE_BUILD}'
export PATH=/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export CARGO_BUILD_JOBS=1
export CARGO_TARGET_DIR='{REMOTE_TARGET}'
cargo build -p mc-feedback-web --release
id feedback-ai >/dev/null 2>&1 || useradd --system --home /var/lib/mc-feedback-web --shell /usr/sbin/nologin feedback-ai
install -d -o root -g root -m 0755 /opt/mc-feedback-web
install -d -o feedback-ai -g feedback-ai -m 0700 /var/lib/mc-feedback-web /var/lib/mc-feedback-web/artifacts
install -d -o root -g feedback-ai -m 0750 /etc/mc-feedback-web
if [ ! -f /etc/mc-feedback-web/master.key ]; then
  umask 0027
  head -c 32 /dev/urandom > /etc/mc-feedback-web/master.key
fi
chown root:feedback-ai /etc/mc-feedback-web/master.key
chmod 0640 /etc/mc-feedback-web/master.key
if [ -x /opt/mc-feedback-web/mc-feedback-web ]; then
  cp -a /opt/mc-feedback-web/mc-feedback-web /opt/mc-feedback-web/mc-feedback-web.previous
fi
install -o root -g root -m 0755 '{REMOTE_TARGET}/release/mc-feedback-web' /opt/mc-feedback-web/mc-feedback-web
install -o root -g root -m 0755 /tmp/mc-feedback-viewer-windows-x64.exe /opt/mc-feedback-web/mc-feedback-viewer-windows-x64.exe
install -o root -g feedback-ai -m 0640 /tmp/mc-feedback-web.env /etc/mc-feedback-web/web.env
sed '/EnvironmentFile=/a Environment=MC_WEB_BIND=127.0.0.1:15678' /tmp/mc-feedback-web.service \
  > /etc/systemd/system/mc-feedback-web-staging.service
systemctl daemon-reload
"""
    run(client, "bash -se", stdin=build, timeout=3600)
    command = (
        "sudo -u feedback-ai sh -c "
        "'set -a; . /etc/mc-feedback-web/web.env; set +a; "
        "exec /opt/mc-feedback-web/mc-feedback-web --bootstrap-stdin'"
    )
    migrate_session(client)
    run(client, command, stdin=bootstrap_payload(), show=False)
    start_stage(client)


def start_stage(client: paramiko.SSHClient) -> None:
    upload(client, ROOT / "deploy/web/mc-feedback-web.service", "/tmp/mc-feedback-web.service")
    upload(client, ROOT / "deploy/web/web.env", "/tmp/mc-feedback-web.env")
    run(
        client,
        "sed 's/127.0.0.1:5678/127.0.0.1:15678/' /tmp/mc-feedback-web.env > /etc/mc-feedback-web/staging.env && "
        "chown root:feedback-ai /etc/mc-feedback-web/staging.env && chmod 0640 /etc/mc-feedback-web/staging.env && "
        "sed 's#EnvironmentFile=/etc/mc-feedback-web/web.env#EnvironmentFile=/etc/mc-feedback-web/staging.env#' /tmp/mc-feedback-web.service > /etc/systemd/system/mc-feedback-web-staging.service && "
        "systemctl daemon-reload && (systemctl reset-failed mc-feedback-web-staging.service 2>/dev/null || true) && "
        "systemctl enable mc-feedback-web-staging.service && systemctl restart mc-feedback-web-staging.service && "
        "for i in $(seq 1 30); do curl -fsS http://127.0.0.1:15678/healthz && exit 0; sleep 1; done; exit 1",
    )
    smoke(client, 15678)


def smoke_script(port: int) -> str:
    email = json.dumps(os.environ["MC_WEB_ADMIN_EMAIL"])
    password = json.dumps(os.environ["MC_WEB_ADMIN_PASSWORD"])
    legacy_account = json.dumps(os.environ.get("MC_WEB_LEGACY_NETEASE_ACCOUNT"))
    return f"""
import json, re, urllib.error, urllib.request
base = "http://127.0.0.1:{port}"
email = {email}
password = {password}
legacy_account = {legacy_account}

def request(path, method="GET", body=None, cookie=None, csrf=None, timeout=300):
    headers = {{"Accept": "application/json"}}
    if body is not None:
        body = json.dumps(body, ensure_ascii=False).encode()
        headers["Content-Type"] = "application/json"
    if cookie:
        headers["Cookie"] = cookie
    if csrf:
        headers["X-CSRF-Token"] = csrf
    req = urllib.request.Request(base + path, data=body, headers=headers, method=method)
    return urllib.request.urlopen(req, timeout=timeout)

login = request("/api/auth/login", "POST", {{"email": email, "password": password}})
login_body = json.loads(login.read())
assert login_body["admin"]["role"] == "admin"
cookie_value = re.search(r"mc_feedback_session=([^;]+)", login.headers["Set-Cookie"]).group(1)
cookie = "mc_feedback_session=" + cookie_value
csrf = login_body["csrf"]

model = json.loads(request("/api/settings/model/test", "POST", {{}}, cookie, csrf).read())
status = json.loads(request("/api/developer/status", cookie=cookie).read())
users = json.loads(request("/api/admin/users", cookie=cookie).read())["items"]
assert model["ok"] and model["tools"]
assert status["session_state"] == "valid"
assert any(user["email"] == email and user["role"] == "admin" for user in users)
if legacy_account:
    assert any(user["email"] == email and user.get("netease_account") == legacy_account for user in users)

conversation = json.loads(request("/api/conversations", "POST", {{"title": "部署只读冒烟"}}, cookie, csrf).read())["item"]
prompt = "必须调用 list_player_comments 读取最新评论，然后用两句话概括；不要读取全部数据。"
stream = request(
    "/api/conversations/" + conversation["id"] + "/messages",
    "POST",
    {{"run_id": "59f0bd88-c385-4baf-8365-8f0b4220b3f1", "content": prompt}},
    cookie,
    csrf,
).read().decode("utf-8")
assert re.search(r"event:\\s*tool", stream) and re.search(r"event:\\s*text", stream) and re.search(r"event:\\s*done", stream)
assert not re.search(r"event:\\s*error", stream)

artifact = json.loads(request(
    "/api/artifacts/export",
    "POST",
    {{"conversation_id": conversation["id"], "format": "docx", "dataset_id": None}},
    cookie,
    csrf,
).read())["artifact"]
download = request(artifact["download_url"], cookie=cookie).read()
assert download.startswith(b"PK") and len(download) > 500
request("/api/artifacts/" + artifact["id"], "DELETE", cookie=cookie, csrf=csrf).read()
request("/api/conversations/" + conversation["id"], "DELETE", cookie=cookie, csrf=csrf).read()
print(json.dumps({{"health": "ok", "model_tools": True, "netease": "valid", "ai_tool": True, "docx": len(download)}}))
"""


def smoke(client: paramiko.SSHClient, port: int) -> None:
    output = run(client, "python3 -", stdin=smoke_script(port), timeout=900, show=False)
    result = json.loads(output.strip().splitlines()[-1])
    print("SMOKE=" + json.dumps(result, ensure_ascii=False))


def cutover(client: paramiko.SSHClient) -> None:
    upload(client, ROOT / "deploy/web/mc-feedback-web.service", "/tmp/mc-feedback-web.service")
    upload(client, ROOT / "deploy/web/web.env", "/tmp/mc-feedback-web.env")
    upload_nginx_config(client)
    script = r"""
set -eux
rollback() {
  code=$?
  set +e
  systemctl disable --now mc-feedback-web.service
  systemctl enable --now mc-feedback-gateway.service feedback-n8n.service
  if [ -f /tmp/nginx-mc-feedback.rollback ]; then
    cp /tmp/nginx-mc-feedback.rollback /etc/nginx/sites-available/mc-feedback
    nginx -t && systemctl reload nginx
  fi
  exit "$code"
}
trap rollback ERR
test -x /opt/mc-feedback-web/mc-feedback-web
if [ -f /etc/nginx/sites-available/mc-feedback ]; then
  cp /etc/nginx/sites-available/mc-feedback /tmp/nginx-mc-feedback.rollback
fi
systemctl stop feedback-n8n.service mc-feedback-gateway.service 2>/dev/null || true
systemctl disable feedback-n8n.service mc-feedback-gateway.service 2>/dev/null || true
systemctl disable --now mc-feedback-web-staging.service 2>/dev/null || true
install -o root -g root -m 0644 /tmp/mc-feedback-web.service /etc/systemd/system/mc-feedback-web.service
install -o root -g feedback-ai -m 0640 /tmp/mc-feedback-web.env /etc/mc-feedback-web/web.env
systemctl daemon-reload
systemctl enable --now mc-feedback-web.service
for i in $(seq 1 30); do curl -fsS http://127.0.0.1:5678/healthz && break; sleep 1; done
curl -fsS http://127.0.0.1:5678/healthz >/dev/null
install -o root -g root -m 0644 /tmp/nginx-mc-feedback.conf /etc/nginx/sites-available/mc-feedback
ln -sfn /etc/nginx/sites-available/mc-feedback /etc/nginx/sites-enabled/mc-feedback
nginx -t
systemctl reload nginx
curl -fsS __PUBLIC_URL__/healthz >/dev/null
trap - ERR
rm -f /tmp/nginx-mc-feedback.rollback
""".replace("__PUBLIC_URL__", public_url())
    run(client, "bash -se", stdin=script)
    smoke(client, 5678)
    print("DOMAIN=" + public_url())


def promote(client: paramiko.SSHClient) -> None:
    try:
        run(
            client,
            "systemctl disable --now mc-feedback-web-staging.service 2>/dev/null || true; "
            "systemctl restart mc-feedback-web.service; "
            "for i in $(seq 1 30); do curl -fsS http://127.0.0.1:5678/healthz && exit 0; sleep 1; done; exit 1",
        )
        smoke(client, 5678)
        run(
            client,
            f"curl -fsS {public_url()}/healthz >/dev/null && "
            "rm -f /opt/mc-feedback-web/mc-feedback-web.previous && "
            "rm -rf /tmp/mc-feedback-web-build && rm -f /tmp/mc-feedback-web-source.tar.gz && "
            "rm -f /etc/systemd/system/mc-feedback-web-staging.service /etc/mc-feedback-web/staging.env && "
            "systemctl daemon-reload",
        )
    except Exception:
        run(
            client,
            "if [ -x /opt/mc-feedback-web/mc-feedback-web.previous ]; then "
            "install -o root -g root -m 0755 /opt/mc-feedback-web/mc-feedback-web.previous /opt/mc-feedback-web/mc-feedback-web; "
            "systemctl restart mc-feedback-web.service; fi",
            show=False,
        )
        raise
    print("PROMOTE=ok")


def reclaim_legacy(client: paramiko.SSHClient) -> None:
    command = (
        "sudo -u feedback-ai sh -c "
        "'set -a; . /etc/mc-feedback-web/web.env; set +a; "
        "exec /opt/mc-feedback-web/mc-feedback-web --bootstrap-stdin'"
    )
    run(client, command, stdin=bootstrap_payload(), show=False)
    audit = run(
        client,
        "python3 -",
        stdin="""
import json
import sqlite3

connection = sqlite3.connect("/var/lib/mc-feedback-web/data.sqlite")
tables = ["conversations", "datasets", "artifacts", "jobs", "job_runs"]
counts = {
    table: connection.execute(
        f"SELECT COUNT(*) FROM {table} WHERE owner_id IS NULL OR owner_id=''"
    ).fetchone()[0]
    for table in tables
}
print(json.dumps(counts))
""",
        show=False,
    )
    counts = json.loads(audit.strip())
    if any(counts.values()):
        raise RuntimeError("unclaimed rows remain: " + json.dumps(counts))
    print("OWNERSHIP=" + json.dumps(counts))


def purge_old(client: paramiko.SSHClient) -> None:
    script = r"""
set -eux
curl -fsS http://127.0.0.1:5678/healthz >/dev/null
curl -fsS __PUBLIC_URL__/healthz >/dev/null
systemctl disable --now feedback-n8n.service mc-feedback-gateway.service 2>/dev/null || true
rm -f /etc/systemd/system/feedback-n8n.service /etc/systemd/system/mc-feedback-gateway.service
rm -rf /opt/feedback-ai
rm -rf /var/lib/feedback-ai
rm -rf /etc/feedback-ai
rm -f /etc/systemd/system/mc-feedback-web-staging.service /etc/mc-feedback-web/staging.env
rm -rf /tmp/mc-feedback-web-build
rm -f /tmp/mc-feedback-web-source.tar.gz /tmp/mc-feedback-web.service /tmp/mc-feedback-web.env /tmp/nginx-mc-feedback.conf
systemctl daemon-reload
systemctl reset-failed
test -x /opt/mc-feedback-viewer/mc-feedback-viewer
test -x /usr/local/bin/opencode
curl -fsS http://127.0.0.1:5678/healthz
""".replace("__PUBLIC_URL__", public_url())
    run(client, "bash -se", stdin=script)
    print("OLD_N8N=removed")


def main() -> None:
    parser = argparse.ArgumentParser()
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--inspect", dest="mode", action="store_const", const="inspect")
    group.add_argument("--stage", dest="mode", action="store_const", const="stage")
    group.add_argument("--resume-stage", dest="mode", action="store_const", const="resume-stage")
    group.add_argument("--cutover", dest="mode", action="store_const", const="cutover")
    group.add_argument("--promote", dest="mode", action="store_const", const="promote")
    group.add_argument("--reclaim", dest="mode", action="store_const", const="reclaim")
    group.add_argument("--purge-old", dest="mode", action="store_const", const="purge-old")
    group.add_argument("--all", dest="mode", action="store_const", const="all")
    args = parser.parse_args()
    with connect() as client:
        if args.mode == "inspect":
            inspect(client)
        elif args.mode == "stage":
            stage(client)
        elif args.mode == "resume-stage":
            bootstrap_payload()
            start_stage(client)
        elif args.mode == "cutover":
            bootstrap_payload()
            cutover(client)
        elif args.mode == "promote":
            bootstrap_payload()
            promote(client)
        elif args.mode == "reclaim":
            reclaim_legacy(client)
        elif args.mode == "purge-old":
            purge_old(client)
        else:
            stage(client)
            cutover(client)
            inspect(client)


if __name__ == "__main__":
    main()
