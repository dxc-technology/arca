# Shared library for Arca bin/ scripts.
# Provides plugin-based Docker Compose and TOML config composition.
#
# Source this file from bin/ scripts:
#   source "$(dirname "$0")/lib/compose.sh"
#
# Usage:
#   enable_tls              # register TLS feature
#   enable_encryption       # register local-key encryption (global)
#   enable_encryption_per_bucket  # register local-key encryption (per-bucket)
#   enable_kms              # register KMS encryption (global)
#   enable_kms_per_bucket   # register KMS encryption (per-bucket)
#   build_config            # concatenate fragments into .generated.toml
#   COMPOSE="$(compose_cmd)" # build docker compose command
#   save_env                # persist feature state for other scripts
#   load_env                # restore feature state from .arca-env

REPO_ROOT="$(git rev-parse --show-toplevel)"
GENERATED_CONFIG="$REPO_ROOT/config/.generated.toml"
ENV_FILE="$REPO_ROOT/.arca-env"
FRAGMENTS_DIR="$REPO_ROOT/config/fragments"

# Feature tracking
_FEATURES=()
_HAS_TLS=false
_HAS_ENCRYPTION=false
_HAS_KMS=false

# --- Feature registration ---

enable_tls() {
    if $_HAS_TLS; then return; fi
    _HAS_TLS=true
    _FEATURES+=(tls)
}

enable_tls_explicit() {
    # For tests: use explicit cert filenames instead of auto-detect.
    # Replaces the tls fragment, so enable_tls must NOT also be called.
    if $_HAS_TLS; then
        echo "Error: cannot combine --tls with tls-explicit (both define [server.tls])" >&2
        exit 1
    fi
    _HAS_TLS=true
    _FEATURES+=(tls-explicit)
}

_check_encryption_conflict() {
    if $_HAS_ENCRYPTION || $_HAS_KMS; then
        echo "Error: --encryption and --kms are mutually exclusive" >&2
        exit 1
    fi
}

enable_encryption() {
    _check_encryption_conflict
    _HAS_ENCRYPTION=true
    _FEATURES+=(encryption)
}

enable_encryption_per_bucket() {
    _check_encryption_conflict
    _HAS_ENCRYPTION=true
    _FEATURES+=(encryption-per-bucket)
}

enable_kms() {
    _check_encryption_conflict
    _HAS_KMS=true
    _FEATURES+=(kms)
}

enable_kms_per_bucket() {
    _check_encryption_conflict
    _HAS_KMS=true
    _FEATURES+=(kms-per-bucket)
}

# --- Config generation ---

build_config() {
    local fragments=("$FRAGMENTS_DIR/base.toml")

    for feat in "${_FEATURES[@]}"; do
        local frag="$FRAGMENTS_DIR/${feat}.toml"
        if [[ ! -f "$frag" ]]; then
            echo "Error: missing fragment: $frag" >&2
            exit 1
        fi
        fragments+=("$frag")
    done

    # Concatenate fragments with blank line separators
    {
        echo "# Auto-generated config -- do not edit."
        echo "# Features: ${_FEATURES[*]:-base}"
        echo "# Generated: $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
        for frag in "${fragments[@]}"; do
            echo ""
            cat "$frag"
        done
    } > "$GENERATED_CONFIG"
}

# --- Compose command building ---

compose_cmd() {
    local files=("$REPO_ROOT/docker/docker-compose.yml")

    for feat in "${_FEATURES[@]}"; do
        case "$feat" in
            tls|tls-explicit)
                # TLS infrastructure: tls-init, cert mounts, console HTTPS
                local tls_file="$REPO_ROOT/docker/docker-compose.tls.yml"
                # Avoid adding it twice (tls and tls-explicit both need it)
                local already=false
                for f in "${files[@]}"; do
                    [[ "$f" == "$tls_file" ]] && already=true
                done
                if ! $already; then
                    files+=("$tls_file")
                fi
                ;;
            kms|kms-per-bucket)
                # KMS infrastructure: openbao, openbao-init
                local kms_file="$REPO_ROOT/docker/docker-compose.kms.yml"
                local already=false
                for f in "${files[@]}"; do
                    [[ "$f" == "$kms_file" ]] && already=true
                done
                if ! $already; then
                    files+=("$kms_file")
                fi
                ;;
        esac
    done

    # If any features are active, mount the generated config
    if [[ ${#_FEATURES[@]} -gt 0 ]]; then
        files+=("$REPO_ROOT/docker/docker-compose.config.yml")
    fi

    local cmd="docker compose"
    for f in "${files[@]}"; do
        cmd="$cmd -f $f"
    done
    echo "$cmd"
}

# --- State persistence ---

save_env() {
    {
        echo "# Arca environment state -- auto-generated, do not edit."
        echo "FEATURES=\"${_FEATURES[*]}\""
        echo "BUILD_TARGET=\"${BUILD_TARGET:-production}\""
    } > "$ENV_FILE"
}

load_env() {
    if [[ ! -f "$ENV_FILE" ]]; then
        # No state file: use base compose (no features)
        return
    fi

    # Source the env file to get FEATURES and BUILD_TARGET
    local saved_features=""
    local saved_build_target=""
    while IFS='=' read -r key value; do
        # Skip comments and empty lines
        [[ "$key" =~ ^#.*$ || -z "$key" ]] && continue
        # Strip quotes from value
        value="${value%\"}"
        value="${value#\"}"
        case "$key" in
            FEATURES)      saved_features="$value" ;;
            BUILD_TARGET)  saved_build_target="$value" ;;
        esac
    done < "$ENV_FILE"

    # Restore BUILD_TARGET
    if [[ -n "$saved_build_target" ]]; then
        export BUILD_TARGET="$saved_build_target"
    fi

    # Re-register features
    for feat in $saved_features; do
        case "$feat" in
            tls)                    enable_tls ;;
            tls-explicit)           enable_tls_explicit ;;
            encryption)             enable_encryption ;;
            encryption-per-bucket)  enable_encryption_per_bucket ;;
            kms)                    enable_kms ;;
            kms-per-bucket)         enable_kms_per_bucket ;;
            custom)                 _FEATURES+=(custom) ;;
        esac
    done
}

clean_env() {
    rm -f "$ENV_FILE"
}

# --- Utilities ---

wait_for_arca() {
    local compose="$1"
    local max_wait="${2:-30}"
    echo "Waiting for Arca server..."
    for i in $(seq 1 "$max_wait"); do
        if $compose exec -T arca /usr/local/bin/arca --help >/dev/null 2>&1; then
            break
        fi
        sleep 1
    done
    sleep 2  # brief extra settle time
}

wait_for_openbao() {
    local compose="$1"
    local max_wait="${2:-30}"
    echo "Waiting for OpenBAO..."
    for i in $(seq 1 "$max_wait"); do
        if $compose exec -T openbao bao status >/dev/null 2>&1; then
            break
        fi
        sleep 1
    done
}
