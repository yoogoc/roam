#!/usr/bin/env bash
#
# Start (or stop) the local servers the integration tests need.
#
# The rest of the suite runs against OpenDAL's local `fs` service, which never
# touches the network. These servers exist so the real protocol paths — request
# signing, XML/JSON parsing, listing pagination, multipart and one-shot uploads,
# presigning, PROPFIND — actually run.
#
#   scripts/test-backends.sh up [backend]     start and print the env to export
#                                             (BARE_ENV=1 drops the `export `
#                                              prefix, for CI's $GITHUB_ENV)
#   scripts/test-backends.sh test [backend]   start, then run that backend's tests
#   scripts/test-backends.sh down             stop and remove everything
#
# backend: s3 | azblob | gcs | webdav | all   (default: all)
#
set -euo pipefail

S3_PORT=19000
AZ_PORT=10000
GCS_PORT=14443
DAV_PORT=18080
BUCKET=roam-test
KEY=roamtest
SECRET=roamtest-secret

# Azurite's well-known development credentials.
AZ_ACCOUNT=devstoreaccount1
AZ_KEY='Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=='

# Deliberately inside the repo rather than under $TMPDIR. On macOS, TMPDIR is
# /var/folders/..., which Docker Desktop does not share by default — the bind
# mount then silently resolves to nothing and MinIO reports
# "Unable to use the drive /data: drive not found". `target/` is already
# gitignored and is under a path Docker can see.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DATA="${ROAM_TEST_DATA:-$REPO_ROOT/target/test-backends}"

require_docker() {
    if ! docker info >/dev/null 2>&1; then
        echo "docker is not running" >&2
        exit 1
    fi
}

wait_for() {
    local what=$1 url=$2 expect=${3:-200}
    # Progress goes to stderr so that `up` can be piped somewhere that expects
    # only variables (CI writes it into $GITHUB_ENV).
    printf 'waiting for %s' "$what" >&2
    for _ in $(seq 1 60); do
        local code
        code=$(curl -s -o /dev/null -w '%{http_code}' "$url" 2>/dev/null || echo 000)
        if [ "$code" = "$expect" ] || { [ "$expect" = "any" ] && [ "$code" != "000" ]; }; then
            echo " ready" >&2
            return 0
        fi
        printf . >&2
        sleep 0.5
    done
    echo " timed out" >&2
    return 1
}

start() {
    local name=$1
    shift

    # Reuse only a *running* container. A stopped one is recreated instead of
    # restarted: `down` deletes the data directory, so an old container's bind
    # mount would point at a path that no longer holds what it expects — MinIO
    # answers that with "Unable to use the drive /data".
    if [ -n "$(docker ps -q -f name="^${name}$")" ]; then
        return 0
    fi
    docker rm -f "$name" >/dev/null 2>&1 || true
    docker run -d --name "$name" "$@" >/dev/null
}

up_s3() {
    # The bucket is created through the S3 API below, not by making a directory
    # under the data dir. MinIO does turn a top-level directory into a bucket,
    # but that only works when we and the daemon see the same filesystem — a
    # runner that executes steps inside a container does not, and the failure
    # arrives much later as NoSuchBucket from every single test.
    mkdir -p "$DATA/minio"
    start roam-minio \
        -p "${S3_PORT}:9000" \
        -e "MINIO_ROOT_USER=$KEY" \
        -e "MINIO_ROOT_PASSWORD=$SECRET" \
        -v "$DATA/minio:/data" \
        minio/minio server /data
    wait_for minio "http://127.0.0.1:${S3_PORT}/minio/health/live"

    # Create the bucket, then turn on versioning so the version-history tests
    # have history to browse. Without versioning they skip, which would look
    # like passing.
    S3KEY="$KEY" S3SECRET="$SECRET" HOSTPORT="127.0.0.1:${S3_PORT}" BUCKET="$BUCKET" \
    python3 - <<'PYEOF'
import datetime, hashlib, hmac, os, sys, urllib.request, urllib.error

key, secret = os.environ["S3KEY"], os.environ["S3SECRET"]
host, bucket = os.environ["HOSTPORT"], os.environ["BUCKET"]
region, service = "us-east-1", "s3"

def sign(k, m):
    return hmac.new(k, m.encode(), hashlib.sha256).digest()

def put(query, body, what, ok_codes=(200,)):
    """Signed PUT on the bucket. `query` is the canonical query string, which
    must be sorted by key — a single flag sorts trivially."""
    now = datetime.datetime.now(datetime.timezone.utc)
    amzdate, datestamp = now.strftime("%Y%m%dT%H%M%SZ"), now.strftime("%Y%m%d")
    payload = hashlib.sha256(body).hexdigest()

    canonical = "\n".join([
        "PUT", f"/{bucket}", query,
        f"host:{host}", f"x-amz-content-sha256:{payload}", f"x-amz-date:{amzdate}", "",
        "host;x-amz-content-sha256;x-amz-date", payload,
    ])
    scope = f"{datestamp}/{region}/{service}/aws4_request"
    to_sign = "\n".join(["AWS4-HMAC-SHA256", amzdate, scope,
                         hashlib.sha256(canonical.encode()).hexdigest()])
    k = sign(("AWS4" + secret).encode(), datestamp)
    for part in (region, service, "aws4_request"):
        k = sign(k, part)
    sig = hmac.new(k, to_sign.encode(), hashlib.sha256).hexdigest()

    url = f"http://{host}/{bucket}" + (f"?{query.rstrip('=')}" if query else "")
    req = urllib.request.Request(url, data=body, method="PUT")
    req.add_header("x-amz-date", amzdate)
    req.add_header("x-amz-content-sha256", payload)
    req.add_header("Authorization",
                   f"AWS4-HMAC-SHA256 Credential={key}/{scope}, "
                   f"SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={sig}")
    try:
        with urllib.request.urlopen(req) as resp:
            print(f"bucket {bucket}: {what} ({resp.status})", file=sys.stderr)
            return True
    except urllib.error.HTTPError as e:
        # A bucket that is already there is not a problem; anything else is, and
        # it has to be loud — every test would otherwise fail with NoSuchBucket
        # and point at the tests rather than at this script.
        if e.code == 409:
            print(f"bucket {bucket}: already exists", file=sys.stderr)
            return True
        print(f"bucket {bucket}: {what} -> {e.code} {e.read()[:200]!r}", file=sys.stderr)
        return False

if not put("", b"", "created"):
    sys.exit(1)
if not put("versioning=",
           b'<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">'
           b'<Status>Enabled</Status></VersioningConfiguration>',
           "versioning enabled"):
    sys.exit(1)
PYEOF
}

up_azblob() {
    start roam-azurite \
        -p "${AZ_PORT}:10000" \
        mcr.microsoft.com/azure-storage/azurite \
        azurite-blob --blobHost 0.0.0.0 --skipApiVersionCheck
    # 403 means it is answering and demanding auth, which is as far as an
    # unsigned request gets.
    wait_for azurite "http://127.0.0.1:${AZ_PORT}/${AZ_ACCOUNT}?comp=list" 403

    # Azurite does not create containers on demand, and OpenDAL will not create
    # one either, so it has to be done here with a signed request.
    ACCOUNT="$AZ_ACCOUNT" AZKEY="$AZ_KEY" HOSTPORT="127.0.0.1:${AZ_PORT}" \
    CONTAINER="$BUCKET" python3 - <<'PYEOF'
import base64, hashlib, hmac, os, sys, urllib.request, urllib.error
from email.utils import formatdate

account, key = os.environ["ACCOUNT"], os.environ["AZKEY"]
host, container = os.environ["HOSTPORT"], os.environ["CONTAINER"]
version = "2021-08-06"

date = formatdate(usegmt=True)
canon_headers = f"x-ms-date:{date}\nx-ms-version:{version}\n"
# Azurite is path-style, so the account appears both in the URI path and in the
# canonicalized-resource prefix.
canon_resource = f"/{account}/{account}/{container}\nrestype:container"
to_sign = f"PUT\n\n\n\n\n\n\n\n\n\n\n\n{canon_headers}{canon_resource}"
sig = base64.b64encode(
    hmac.new(base64.b64decode(key), to_sign.encode(), hashlib.sha256).digest()
).decode()

req = urllib.request.Request(
    f"http://{host}/{account}/{container}?restype=container", method="PUT"
)
req.add_header("x-ms-date", date)
req.add_header("x-ms-version", version)
req.add_header("Authorization", f"SharedKey {account}:{sig}")
req.add_header("Content-Length", "0")
try:
    with urllib.request.urlopen(req) as resp:
        print(f"container {container}: created ({resp.status})", file=sys.stderr)
except urllib.error.HTTPError as e:
    # 409 is "already there", which is fine.
    print(f"container {container}: {'already exists' if e.code == 409 else e.code}", file=sys.stderr)
PYEOF
}

up_gcs() {
    # The bucket is created through the JSON API rather than by making a
    # directory under the data root. fake-gcs-server does adopt a directory as a
    # bucket, but only when it and we share a filesystem — see up_s3.
    mkdir -p "$DATA/gcs"
    start roam-gcs \
        -p "${GCS_PORT}:4443" \
        -v "$DATA/gcs:/data" \
        fsouza/fake-gcs-server \
        -scheme http -port 4443 -backend filesystem -filesystem-root /data
    wait_for fake-gcs "http://127.0.0.1:${GCS_PORT}/storage/v1/b"

    local code
    code=$(curl -s -o /dev/null -w '%{http_code}' -X POST \
                -H 'Content-Type: application/json' \
                -d "{\"name\":\"${BUCKET}\"}" \
                "http://127.0.0.1:${GCS_PORT}/storage/v1/b?project=roam-test")
    case "$code" in
        200|409) echo "bucket ${BUCKET}: gcs ready (${code})" >&2 ;;
        *)       echo "bucket ${BUCKET}: gcs create -> ${code}" >&2; return 1 ;;
    esac
}

up_webdav() {
    mkdir -p "$DATA/dav"
    start roam-dav \
        -p "${DAV_PORT}:80" \
        -e "USERNAME=$KEY" \
        -e "PASSWORD=$SECRET" \
        -v "$DATA/dav:/var/lib/dav/data" \
        bytemark/webdav
    wait_for webdav "http://${KEY}:${SECRET}@127.0.0.1:${DAV_PORT}/" any
}

env_block() {
    cat <<EOF
export ROAM_S3_ENDPOINT=http://127.0.0.1:${S3_PORT}
export ROAM_S3_BUCKET=${BUCKET}
export ROAM_S3_KEY=${KEY}
export ROAM_S3_SECRET=${SECRET}
export ROAM_AZBLOB_ENDPOINT=http://127.0.0.1:${AZ_PORT}/${AZ_ACCOUNT}
export ROAM_AZBLOB_CONTAINER=${BUCKET}
export ROAM_AZBLOB_ACCOUNT=${AZ_ACCOUNT}
export ROAM_AZBLOB_KEY='${AZ_KEY}'
export ROAM_GCS_ENDPOINT=http://127.0.0.1:${GCS_PORT}
export ROAM_GCS_BUCKET=${BUCKET}
export ROAM_WEBDAV_ENDPOINT=http://127.0.0.1:${DAV_PORT}
export ROAM_WEBDAV_USER=${KEY}
export ROAM_WEBDAV_PASSWORD=${SECRET}
EOF
}

# `export KEY=value` for a human to `eval`; bare `KEY=value` for CI to append to
# $GITHUB_ENV, which does not accept `export`.
#
# The single quotes around the azblob key are there so a shell `eval` does not
# choke on its `/` and `+` characters. $GITHUB_ENV takes values literally, so
# they have to come off — otherwise the quotes become part of the key and
# authentication fails with a signature error that looks nothing like the cause.
bare_env_block() {
    env_block | sed -e 's/^export //' -e "s/='\(.*\)'$/=\1/"
}

BACKEND=${2:-all}

case "${1:-up}" in
    up|test)
        require_docker
        case "$BACKEND" in
            s3) up_s3 ;;
            azblob) up_azblob ;;
            gcs) up_gcs ;;
            webdav) up_webdav ;;
            all) up_s3; up_azblob; up_gcs; up_webdav ;;
            *) echo "unknown backend: $BACKEND" >&2; exit 2 ;;
        esac

        if [ "${1}" = "up" ]; then
            if [ "${BARE_ENV:-0}" = "1" ]; then
                bare_env_block
            else
                env_block
            fi
            exit 0
        fi

        # shellcheck disable=SC2046
        eval "$(env_block)"

        # Serialised: the tests share one bucket under per-test prefixes, and the
        # S3 paging test alone uploads a thousand objects.
        if [ "$BACKEND" = "s3" ] || [ "$BACKEND" = "all" ]; then
            cargo test -p roam-core --test s3 -- --test-threads=1
            cargo test -p roam-ui s3_tests
        fi
        if [ "$BACKEND" != "s3" ]; then
            cargo test -p roam-core --test backends -- --test-threads=1
        fi
        ;;
    down)
        docker rm -f roam-minio roam-azurite roam-gcs roam-dav \
            >/dev/null 2>&1 || true
        rm -rf "$DATA"
        echo "stopped and removed the test backends"
        ;;
    *)
        echo "usage: $0 {up|test|down} [s3|azblob|gcs|webdav|all]" >&2
        exit 2
        ;;
esac
