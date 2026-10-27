#!/bin/bash
# Install the ECX bridge on a dedicated Ubuntu Server.
# Prepare first: docs/prepare.md. This script does not create wallets.
set -euo pipefail
umask 077
# Name the step when a command fails. set -e alone exits silently, which once hid a permission
# error inside a command substitution. $BASH_COMMAND is the unexpanded text, so no secret is shown.
set -E
trap 'on_error "$LINENO" "$BASH_COMMAND"' ERR

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ETC="/etc/ecash-bridge"
OPT="/opt/ecash-bridge"
ROLLBACK_SERVICES=0
SERVICES_WERE_ACTIVE=0
RUN_ENV=""
NODE_VERSION="v22.23.4"
NODE_SHA_X64="1805bcd35e6815559bd2eb97db438d3fdb0eb3a5e6a977b7c1badca76337e9ce"
NODE_SHA_ARM64="6287aa46b6c375d816e63ae66d024fbbfd70404825aabdfdf898a970871d2f4b"
EXAMPLE_FILE="$ROOT/params.conf.example"
BUNDLE="$ROOT/bundle"
ASSIST="$OPT/assistant"
# Keys whose value in params.conf.example only shows the shape of a real one. A filled file that
# still carries that sample is refused. Keys with working defaults (mint, fees, the ECX explorer
# URL) are not listed. SOLANA_RPC_URL is listed: the sample is not a working endpoint.
MUST_REPLACE_KEYS=(
	WALLET_MNEMONIC WALLET_PASSPHRASE SOLANA_KEYPAIR OWNER_ECX_ADDRESS OWNER_SOL_ADDRESS
	VPN_WG_PRIVATE_KEY VPN_WG_ADDRESS VPN_WG_DNS VPN_WG_PEER_PUBLIC_KEY VPN_WG_ENDPOINT VPN_PUBLIC_IPV4
	HOP1_WG_PRIVATE_KEY HOP1_WG_ADDRESS HOP1_WG_DNS HOP1_WG_PEER_PUBLIC_KEY HOP1_WG_ENDPOINT
	CLOUDFLARE_TUNNEL_TOKEN CLOUDFLARE_HOSTNAME SOLANA_RPC_URL
)
declare -A PARAM_VALUES=() EXAMPLE_VALUES=()
NEW_BRIDGE_HINT="Restore the previous value, or, to run a different bridge on this machine, settle every order and move the funds out first, then move $OPT/backend/data and /var/lib/ecash-bridge/deposit-addresses.txt aside."

die() {
	printf 'install.sh: %s\n' "$1" >&2
	((ROLLBACK_SERVICES)) && rollback_services
	exit 1
}
warn() { printf 'install.sh: warning: %s\n' "$1" >&2; }
on_error() {
	printf 'install.sh: failed at line %s: %s\n' "$1" "$2" >&2
	((ROLLBACK_SERVICES)) && rollback_services
	return 0
}

usage() {
	cat <<'EOF'
Usage:
  ./install.sh check /path/to/params.conf
  sudo ./install.sh /path/to/params.conf
  sudo ./install.sh status
  sudo ./install.sh addresses
  sudo ./install.sh enable --i-deposited

install.sh installs from ./bundle, made by make-bundle.sh on a machine with
internet, and downloads nothing itself. The server may be offline during the
install; its first connection is then the VPN handshake.
check does not install and does not print secrets. It refuses sample values
left over from params.conf.example.
The first install leaves payouts off and does not start the public tunnel.
Deposit both treasuries, then run enable.
EOF
}

reject_chars() {
	local key="$1" value="$2"
	case "$value" in
		*\'*|*\"*|*'\'*|*'$'*|*'`'*) die "$key contains a character the installer will not accept" ;;
	esac
	case "$value" in
		*\\*) die "$key contains a character the installer will not accept" ;;
	esac
}

# Reads KEY = "value" lines. By default each key becomes a shell variable and a PARAM_VALUES
# entry. With a second argument of "example", values go only into EXAMPLE_VALUES, so the shipped
# params.conf.example can be read for comparison without touching the operator's values.
parse_params() {
	local file="$1" target="${2:-params}" line key value
	[[ -f "$file" ]] || die "params file not found: $file"
	[[ -r "$file" ]] || die "params file is not readable"
	while IFS= read -r line || [[ -n "$line" ]]; do
		case "$line" in
			''|[[:space:]]\#*) continue ;;
		esac
		[[ "$line" =~ ^[[:space:]]*# ]] && continue
		[[ "$line" =~ ^[[:space:]]*$ ]] && continue
		[[ "$line" =~ ^([A-Z][A-Z0-9_]*)[[:space:]]*=[[:space:]]*(.*)$ ]] || die "bad params line (expected KEY = \"value\")"
		key="${BASH_REMATCH[1]}"
		value="${BASH_REMATCH[2]}"
		value="${value%"${value##*[![:space:]]}"}"
		if [[ "$value" =~ ^\"(.*)\"[[:space:]]*(#.*)?$ ]]; then
			value="${BASH_REMATCH[1]}"
		fi
		case "$key" in
			WALLET_MNEMONIC|WALLET_PASSPHRASE|SOLANA_KEYPAIR|WECX_MINT_ADDRESS|ASSET_TICKER|\
			OWNER_ECX_ADDRESS|OWNER_SOL_ADDRESS|EARNINGS_THRESHOLD_ECX|EARNINGS_THRESHOLD_WECX|\
			PEGIN_FEE_BPS|PEGOUT_FEE_BPS|PEGIN_FLAT_FEE_SAT|MIN_DEPOSIT_AMOUNT_SAT|MAX_DEPOSIT_AMOUNT_SAT|ECX_DEPOSIT_MIN_CONFS|\
			UNMATCHED_DEPOSIT_REFUNDS|UNMATCHED_DEPOSIT_FEE_BPS|\
			ORDER_LIFETIME_MINUTES|MAX_PENDING_ORDERS|\
			SOL_STIPEND_LAMPORTS|SOL_STIPEND_PRICE_SAT|SOL_STIPEND_USD_LABEL|\
			ECX_ESPLORA_URL|SOLANA_RPC_URL|ECX_EXPLORER_TX_URL|SOLANA_EXPLORER_TX_URL|\
			VPN_PROVIDER|VPN_WG_PRIVATE_KEY|VPN_WG_ADDRESS|VPN_WG_DNS|VPN_WG_PEER_PUBLIC_KEY|VPN_WG_ENDPOINT|VPN_WG_ALLOWED_IPS|VPN_PUBLIC_IPV4|\
			HOP1_PROVIDER|HOP1_WG_PRIVATE_KEY|HOP1_WG_ADDRESS|HOP1_WG_DNS|HOP1_WG_PEER_PUBLIC_KEY|HOP1_WG_ENDPOINT|HOP1_WG_ALLOWED_IPS|\
			CLOUDFLARE_TUNNEL_TOKEN|CLOUDFLARE_HOSTNAME) ;;
			*) die "unknown params key: $key" ;;
		esac
		reject_chars "$key" "$value"
		if [[ "$target" == example ]]; then
			EXAMPLE_VALUES["$key"]="$value"
		else
			printf -v "$key" '%s' "$value"
			PARAM_VALUES["$key"]="$value"
		fi
	done <"$file"
}

# Every secret and address in params.conf.example is a sample, some cut short with "...". A sample
# that was copied but never replaced must not reach the validators below, whose messages would
# describe the wrong problem, and must never reach a generated config file.
reject_unreplaced_examples() {
	local key example
	[[ -r "$EXAMPLE_FILE" ]] || return 0
	parse_params "$EXAMPLE_FILE" example
	for key in "${MUST_REPLACE_KEYS[@]}"; do
		example="${EXAMPLE_VALUES[$key]-}"
		[[ -n "$example" && "${PARAM_VALUES[$key]-}" == "$example" ]] || continue
		die "$key is still the sample value from params.conf.example. Replace it with your own."
	done
}

sha256_text() {
	printf '%s' "$1" | sha256sum | awk '{print $1}'
}

reject_known_secret() {
	local key="$1" value="$2" digest
	[[ -n "$value" ]] || return 0
	digest="$(sha256_text "$value")"
	case "$digest" in
		2bf92ebff61e4d459a1b6cc2c77de7c5f321bfb01d83215f198425cbd5567fb7|\
		556d56597da1132a1e562e981d9d77f9b49e2ff22d53daf9b20946ce1cfa7498|\
		c557eec878dfd852ba3f88087c4f350f09c55537ab5e549c3cd14320ec3cef38|\
		68c6ab3eba220da70c0fc012ba5f1b6107a8b5a24a4a7b7354984d6e7aa2137b|\
		d7e087650d5a7c895e4c57daab23770777a5f18ae013590a72a646ced11194dd)
			die "$key is an example or a secret that was committed to git. Generate a new one."
			;;
	esac
}

is_ipv4() {
	[[ "$1" =~ ^([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})$ ]] || return 1
	local octet
	for octet in "${BASH_REMATCH[@]:1}"; do
		((10#$octet <= 255)) || return 1
	done
}

is_port() { [[ "$1" =~ ^[0-9]+$ ]] && ((10#$1 >= 1 && 10#$1 <= 65535)); }

is_wg_key() { [[ "$1" =~ ^[A-Za-z0-9+/]{42,44}=$ ]]; }

is_wg_address() { [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+/[0-9]+$ ]]; }

# A WireGuard file may put several comma-separated values on one line: an IPv4 and an IPv6
# address, or two DNS servers. IPv6 is disabled on this machine, so the first IPv4 entry is the
# one that is used. Prints it, or fails if there is none.
first_ipv4_entry() {
	local raw="$1" kind="$2" entry
	local -a entries
	IFS=',' read -ra entries <<<"$raw"
	for entry in "${entries[@]}"; do
		entry="${entry//[[:space:]]/}"
		case "$kind" in
			address) is_wg_address "$entry" && { printf '%s\n' "$entry"; return 0; } ;;
			dns) is_ipv4 "$entry" && { printf '%s\n' "$entry"; return 0; } ;;
		esac
	done
	return 1
}

need() {
	local key="$1" value="$2"
	[[ -n "$value" ]] || die "$key is empty"
}

validate_params() {
	local words=() word
	reject_unreplaced_examples
	need WALLET_MNEMONIC "${WALLET_MNEMONIC:-}"
	need SOLANA_KEYPAIR "${SOLANA_KEYPAIR:-}"
	need WECX_MINT_ADDRESS "${WECX_MINT_ADDRESS:-}"
	need OWNER_ECX_ADDRESS "${OWNER_ECX_ADDRESS:-}"
	need OWNER_SOL_ADDRESS "${OWNER_SOL_ADDRESS:-}"
	need VPN_WG_PRIVATE_KEY "${VPN_WG_PRIVATE_KEY:-}"
	need VPN_WG_ADDRESS "${VPN_WG_ADDRESS:-}"
	need VPN_WG_DNS "${VPN_WG_DNS:-}"
	need VPN_WG_PEER_PUBLIC_KEY "${VPN_WG_PEER_PUBLIC_KEY:-}"
	need VPN_WG_ENDPOINT "${VPN_WG_ENDPOINT:-}"
	need VPN_PUBLIC_IPV4 "${VPN_PUBLIC_IPV4:-}"
	need CLOUDFLARE_TUNNEL_TOKEN "${CLOUDFLARE_TUNNEL_TOKEN:-}"
	need CLOUDFLARE_HOSTNAME "${CLOUDFLARE_HOSTNAME:-}"

	if [[ -n "${ASSET_TICKER:-}" ]]; then
		[[ "$ASSET_TICKER" =~ ^[A-Za-z][A-Za-z0-9]{0,15}$ ]] || die "ASSET_TICKER must be 1-16 letters and digits, starting with a letter"
	else
		ASSET_TICKER=bECX
	fi

	reject_known_secret WALLET_MNEMONIC "$WALLET_MNEMONIC"
	reject_known_secret SOLANA_KEYPAIR "$SOLANA_KEYPAIR"
	# shellcheck disable=SC2206
	words=($WALLET_MNEMONIC)
	((${#words[@]} == 12 || ${#words[@]} == 24)) || die "WALLET_MNEMONIC must be 12 or 24 words"
	for word in "${words[@]}"; do
		[[ "$word" =~ ^[a-z]+$ ]] || die "WALLET_MNEMONIC has a word that is not lowercase letters"
	done
	[[ "$SOLANA_KEYPAIR" =~ ^[1-9A-HJ-NP-Za-km-z]{80,90}$ ]] || die "SOLANA_KEYPAIR is not a base58 64-byte secret"
	[[ "$WECX_MINT_ADDRESS" =~ ^[1-9A-HJ-NP-Za-km-z]{32,44}$ ]] || die "WECX_MINT_ADDRESS is not a Solana address"
	[[ "$OWNER_SOL_ADDRESS" =~ ^[1-9A-HJ-NP-Za-km-z]{32,44}$ ]] || die "OWNER_SOL_ADDRESS is not a Solana address"
	[[ "$OWNER_ECX_ADDRESS" =~ ^bc1q[a-z0-9]{38,59}$ ]] || die "OWNER_ECX_ADDRESS must be a bc1q address"
	# Whether the owner addresses belong to the hot wallets can only be checked once the wallets
	# exist on the server - see the derivation checks in cmd_install.

	is_wg_key "$VPN_WG_PRIVATE_KEY" || die "VPN_WG_PRIVATE_KEY is not a WireGuard key"
	is_wg_key "$VPN_WG_PEER_PUBLIC_KEY" || die "VPN_WG_PEER_PUBLIC_KEY is not a WireGuard key"
	endpoint_is_ip "$VPN_WG_ENDPOINT" || die "VPN_WG_ENDPOINT must be an IP address and port, such as 203.0.113.10:51820. The server may have no DNS when it installs. Resolve a hostname once on another machine: getent ahostsv4 <hostname>"
	VPN_WG_ADDRESS="$(first_ipv4_entry "$VPN_WG_ADDRESS" address)" || die "VPN_WG_ADDRESS must contain an IPv4 address like 10.0.0.2/32"
	VPN_WG_DNS="$(first_ipv4_entry "$VPN_WG_DNS" dns)" || die "VPN_WG_DNS must contain an IPv4 address"
	is_ipv4 "$VPN_PUBLIC_IPV4" || die "VPN_PUBLIC_IPV4 must be an IPv4 address"
	[[ "${VPN_WG_ALLOWED_IPS:-0.0.0.0/0, ::/0}" == "0.0.0.0/0, ::/0" ]] || die "VPN_WG_ALLOWED_IPS must be 0.0.0.0/0, ::/0"
	# The provider names are labels for the operator's own records; the installer treats every
	# provider as a WireGuard file plus, for hop 2, a dedicated address.
	[[ -z "${VPN_PROVIDER:-}" || "${VPN_PROVIDER}" =~ ^[a-z][a-z0-9-]{0,31}$ ]] || die "VPN_PROVIDER must be a short lowercase name such as surfshark"

	[[ "$CLOUDFLARE_TUNNEL_TOKEN" =~ ^eyJ[A-Za-z0-9+/=_-]+$ ]] || die "CLOUDFLARE_TUNNEL_TOKEN must be only the eyJ string: no spaces or dots, not the install command"
	[[ "$CLOUDFLARE_HOSTNAME" != *swap.beta.ecash.com* ]] || die "CLOUDFLARE_HOSTNAME already leaked an address. Use a new name."
	[[ "$CLOUDFLARE_HOSTNAME" =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?){1,3}$ ]] || die "CLOUDFLARE_HOSTNAME is not a lowercase hostname of two to four labels, such as swap.example.com"
	# Cloudflare's free certificate covers the zone apex and one label under it. Four labels is
	# still one label under a zone such as example.co.uk, so it is allowed with a note; the
	# installer cannot tell where the zone apex is.
	if [[ "$CLOUDFLARE_HOSTNAME" == *.*.*.* ]]; then
		warn "CLOUDFLARE_HOSTNAME has four labels. That is fine under a country domain such as example.co.uk. If the zone is example.com, a name like a.b.example.com is not covered by the free certificate and needs Cloudflare's paid Advanced Certificate."
	fi

	[[ "${ECX_ESPLORA_URL:-https://explorer.beta.ecash.ninja/api/}" == https://* ]] || die "ECX_ESPLORA_URL must start with https://"
	need SOLANA_RPC_URL "${SOLANA_RPC_URL:-}"
	[[ "$SOLANA_RPC_URL" == https://* ]] || die "SOLANA_RPC_URL must be an https URL. See deploy/edge/solana-rpc.md"
	case "$SOLANA_RPC_URL" in
		*api.mainnet-beta.solana.com*|*api.devnet.solana.com*|*api.testnet.solana.com*)
			die "SOLANA_RPC_URL is a public Solana endpoint. Buy a paid RPC. See deploy/edge/solana-rpc.md" ;;
	esac
	[[ "$SOLANA_RPC_URL" != *replace-this-paid-key* ]] || die "SOLANA_RPC_URL is still the sample from params.conf.example. See deploy/edge/solana-rpc.md"
	[[ -z "${ECX_EXPLORER_TX_URL:-}" || "${ECX_EXPLORER_TX_URL}" == https://* ]] || die "ECX_EXPLORER_TX_URL must be empty or start with https://"
	[[ -z "${SOLANA_EXPLORER_TX_URL:-}" || "${SOLANA_EXPLORER_TX_URL}" == https://* ]] || die "SOLANA_EXPLORER_TX_URL must be empty or start with https://"
	[[ "${PEGIN_FEE_BPS:-100}" =~ ^[0-9]+$ ]] || die "PEGIN_FEE_BPS must be an integer"
	[[ "${PEGOUT_FEE_BPS:-100}" =~ ^[0-9]+$ ]] || die "PEGOUT_FEE_BPS must be an integer"
	[[ "${PEGIN_FLAT_FEE_SAT:-1000}" =~ ^[0-9]+$ ]] || die "PEGIN_FLAT_FEE_SAT must be an integer"
	[[ "${MIN_DEPOSIT_AMOUNT_SAT:-500}" =~ ^[0-9]+$ ]] || die "MIN_DEPOSIT_AMOUNT_SAT must be an integer"
	[[ "${MAX_DEPOSIT_AMOUNT_SAT:-2100000000000000}" =~ ^[0-9]+$ ]] || die "MAX_DEPOSIT_AMOUNT_SAT must be an integer"
	(( ${MAX_DEPOSIT_AMOUNT_SAT:-2100000000000000} > ${MIN_DEPOSIT_AMOUNT_SAT:-500} )) || die "MAX_DEPOSIT_AMOUNT_SAT must be above MIN_DEPOSIT_AMOUNT_SAT"
	[[ "${ECX_DEPOSIT_MIN_CONFS:-3}" =~ ^[0-9]+$ ]] || die "ECX_DEPOSIT_MIN_CONFS must be an integer"
	(( ${ECX_DEPOSIT_MIN_CONFS:-3} >= 1 )) || die "ECX_DEPOSIT_MIN_CONFS must be at least 1"
	[[ "${UNMATCHED_DEPOSIT_REFUNDS:-1}" =~ ^[01]$ ]] || die "UNMATCHED_DEPOSIT_REFUNDS must be 1 or 0"
	[[ "${UNMATCHED_DEPOSIT_FEE_BPS:-100}" =~ ^[0-9]+$ ]] || die "UNMATCHED_DEPOSIT_FEE_BPS must be an integer"
	(( ${UNMATCHED_DEPOSIT_FEE_BPS:-100} <= 10000 )) || die "UNMATCHED_DEPOSIT_FEE_BPS must be at most 10000"
	[[ "${ORDER_LIFETIME_MINUTES:-15}" =~ ^[0-9]+$ ]] || die "ORDER_LIFETIME_MINUTES must be an integer"
	(( ${ORDER_LIFETIME_MINUTES:-15} >= 1 && ${ORDER_LIFETIME_MINUTES:-15} <= 1440 )) || die "ORDER_LIFETIME_MINUTES must be between 1 and 1440"
	[[ "${MAX_PENDING_ORDERS:-500}" =~ ^[0-9]+$ ]] || die "MAX_PENDING_ORDERS must be an integer"
	(( ${MAX_PENDING_ORDERS:-500} >= 1 )) || die "MAX_PENDING_ORDERS must be at least 1"
	[[ "${SOL_STIPEND_LAMPORTS:-5000000}" =~ ^[0-9]+$ ]] || die "SOL_STIPEND_LAMPORTS must be an integer"
	[[ "${SOL_STIPEND_PRICE_SAT:-500000}" =~ ^[0-9]+$ ]] || die "SOL_STIPEND_PRICE_SAT must be an integer"
	if (( ${SOL_STIPEND_LAMPORTS:-5000000} == 0 && ${SOL_STIPEND_PRICE_SAT:-500000} == 0 )); then
		:
	elif (( ${SOL_STIPEND_LAMPORTS:-5000000} >= 1000000 && ${SOL_STIPEND_PRICE_SAT:-500000} >= 1 )); then
		:
	else
		die "SOL stipend must be both 0, or at least 1000000 lamports and a positive ECX price"
	fi
	[[ -z "${SOL_STIPEND_USD_LABEL:-0.50}" || "${SOL_STIPEND_USD_LABEL:-0.50}" =~ ^[0-9]+(\.[0-9]{1,2})?$ ]] || die "SOL_STIPEND_USD_LABEL must look like 0.50"
	[[ "${EARNINGS_THRESHOLD_ECX:-100}" =~ ^[0-9]+(\.[0-9]+)?$ ]] || die "EARNINGS_THRESHOLD_ECX must be a positive amount"
	[[ "${EARNINGS_THRESHOLD_WECX:-100}" =~ ^[0-9]+(\.[0-9]+)?$ ]] || die "EARNINGS_THRESHOLD_WECX must be a positive amount"

	HOP1_ON=0
	# The provider label alone does not switch hop 1 on; the sample file names Mullvad there while
	# leaving the WireGuard lines empty. Any filled WireGuard line does, and then all are required.
	if [[ -n "${HOP1_WG_PRIVATE_KEY:-}${HOP1_WG_ADDRESS:-}${HOP1_WG_DNS:-}${HOP1_WG_PEER_PUBLIC_KEY:-}${HOP1_WG_ENDPOINT:-}" ]]; then
		HOP1_ON=1
		need HOP1_WG_PRIVATE_KEY "${HOP1_WG_PRIVATE_KEY:-}"
		need HOP1_WG_ADDRESS "${HOP1_WG_ADDRESS:-}"
		need HOP1_WG_DNS "${HOP1_WG_DNS:-}"
		need HOP1_WG_PEER_PUBLIC_KEY "${HOP1_WG_PEER_PUBLIC_KEY:-}"
		need HOP1_WG_ENDPOINT "${HOP1_WG_ENDPOINT:-}"
		is_wg_key "$HOP1_WG_PRIVATE_KEY" || die "HOP1_WG_PRIVATE_KEY is not a WireGuard key"
		is_wg_key "$HOP1_WG_PEER_PUBLIC_KEY" || die "HOP1_WG_PEER_PUBLIC_KEY is not a WireGuard key"
		endpoint_is_ip "$HOP1_WG_ENDPOINT" || die "HOP1_WG_ENDPOINT must be an IP address and port, such as 198.51.100.20:51820"
		HOP1_WG_ADDRESS="$(first_ipv4_entry "$HOP1_WG_ADDRESS" address)" || die "HOP1_WG_ADDRESS must contain an IPv4 address like 10.0.0.2/32"
		HOP1_WG_DNS="$(first_ipv4_entry "$HOP1_WG_DNS" dns)" || die "HOP1_WG_DNS must contain an IPv4 address"
		[[ "${HOP1_WG_ALLOWED_IPS:-0.0.0.0/0, ::/0}" == "0.0.0.0/0, ::/0" ]] || die "HOP1_WG_ALLOWED_IPS must be 0.0.0.0/0, ::/0"
		[[ -z "${HOP1_PROVIDER:-}" || "${HOP1_PROVIDER}" =~ ^[a-z][a-z0-9-]{0,31}$ ]] || die "HOP1_PROVIDER must be a short lowercase name such as mullvad"
	fi
}

endpoint_is_ip() {
	local host="${1%:*}" port="${1##*:}"
	[[ "$host" != "$1" ]] && is_ipv4 "$host" && is_port "$port"
}

split_endpoint() {
	local endpoint="$1" host port
	host="${endpoint%:*}"
	port="${endpoint##*:}"
	[[ "$host" != "$endpoint" && -n "$host" ]] || die "endpoint must be host:port"
	is_port "$port" || die "endpoint port is invalid: $endpoint"
	printf '%s %s\n' "$host" "$port"
}

resolve_v4() {
	local host="$1" ip
	if is_ipv4 "$host"; then
		printf '%s\n' "$host"
		return
	fi
	[[ "$host" =~ ^[A-Za-z0-9.-]+$ ]] || die "endpoint host has unexpected characters"
	ip="$(getent ahostsv4 "$host" | awk '{print $1; exit}')"
	is_ipv4 "${ip:-}" || die "could not resolve $host to an IPv4 address"
	printf '%s\n' "$ip"
}

write_env_file() {
	local path="$1"
	umask 077
	cat >"$path" <<EOF
WALLET_MNEMONIC="$WALLET_MNEMONIC"
WALLET_PASSPHRASE="${WALLET_PASSPHRASE:-}"
SOLANA_KEYPAIR="$SOLANA_KEYPAIR"
WECX_MINT_ADDRESS="$WECX_MINT_ADDRESS"
ASSET_TICKER="${ASSET_TICKER:-bECX}"
ECX_ESPLORA_URL="${ECX_ESPLORA_URL:-https://explorer.beta.ecash.ninja/api/}"
SOLANA_RPC_URL="$SOLANA_RPC_URL"
SOLANA_EXPLORER_TX_URL="${SOLANA_EXPLORER_TX_URL:-https://solscan.io/tx}"
PEGIN_FEE_BPS="${PEGIN_FEE_BPS:-100}"
PEGOUT_FEE_BPS="${PEGOUT_FEE_BPS:-100}"
PEGIN_FLAT_FEE_SAT="${PEGIN_FLAT_FEE_SAT:-1000}"
MIN_DEPOSIT_AMOUNT_SAT="${MIN_DEPOSIT_AMOUNT_SAT:-500}"
MAX_DEPOSIT_AMOUNT_SAT="${MAX_DEPOSIT_AMOUNT_SAT:-2100000000000000}"
ECX_DEPOSIT_MIN_CONFS="${ECX_DEPOSIT_MIN_CONFS:-3}"
UNMATCHED_DEPOSIT_REFUNDS="${UNMATCHED_DEPOSIT_REFUNDS:-1}"
UNMATCHED_DEPOSIT_FEE_BPS="${UNMATCHED_DEPOSIT_FEE_BPS:-100}"
ORDER_LIFETIME_MINUTES="${ORDER_LIFETIME_MINUTES:-15}"
MAX_PENDING_ORDERS="${MAX_PENDING_ORDERS:-500}"
SOL_STIPEND_LAMPORTS="${SOL_STIPEND_LAMPORTS:-5000000}"
SOL_STIPEND_PRICE_SAT="${SOL_STIPEND_PRICE_SAT:-500000}"
SOL_STIPEND_USD_LABEL="${SOL_STIPEND_USD_LABEL:-0.50}"
ALLOW_LIVE_PAYOUTS=${WAS_LIVE:-0}
DATABASE_PATH=$OPT/backend/data/bridge.db
EARNINGS_CONFIG_PATH=$ETC/earnings.toml
EOF
	# An empty ECX_EXPLORER_TX_URL in params means "derive from the Esplora URL"; the backend reads
	# an explicitly empty variable as "no links", so the line is only written when set.
	if [[ -n "${ECX_EXPLORER_TX_URL:-}" ]]; then
		printf 'ECX_EXPLORER_TX_URL="%s"\n' "$ECX_EXPLORER_TX_URL" >> "$path"
	fi
	chmod 600 "$path"
}

write_earnings() {
	cat >"$ETC/earnings.toml" <<EOF
enabled = true
earnings_threshold_wbecx = "${EARNINGS_THRESHOLD_WECX:-100}"
earnings_threshold_becx = "${EARNINGS_THRESHOLD_ECX:-100}"
solana_owner_address = "$OWNER_SOL_ADDRESS"
ecx_owner_address = "$OWNER_ECX_ADDRESS"
EOF
	# The backend service user reads this file at startup. It holds thresholds and the owner
	# addresses, no keys. The directory is traversable for that; the secret files in it stay 0600.
	chown root:bridgebackend "$ETC/earnings.toml"
	chmod 640 "$ETC/earnings.toml"
}

write_wg() {
	local name="$1" private="$2" address="$3" peer="$4" endpoint_ip="$5" endpoint_port="$6" extra="$7"
	local path="/etc/wireguard/${name}.conf"
	umask 077
	cat >"$path" <<EOF
[Interface]
PrivateKey = $private
Address = $address
$extra
[Peer]
PublicKey = $peer
Endpoint = ${endpoint_ip}:${endpoint_port}
AllowedIPs = 0.0.0.0/0
PersistentKeepalive = 25
EOF
	chmod 600 "$path"
}

collect_ssh_peers() {
	SSH_PEERS=()
	local peer
	command -v ss >/dev/null || return 0
	while read -r peer; do
		[[ -n "$peer" ]] || continue
		if [[ "$peer" == *:* ]]; then
			die "an SSH session is using IPv6. Reconnect over IPv4. The installer disables IPv6."
		fi
		is_ipv4 "$peer" || continue
		SSH_PEERS+=("$peer")
	done < <(ss -H -tn state established '( sport = :22 )' | awk '{print $NF}' | sed 's/:[0-9]*$//' | sort -u || true)
}

write_policy_scripts() {
	local peer prio=60
	[[ "$PHYS" =~ ^[A-Za-z0-9._:-]+$ ]] || die "unexpected network interface name"
	[[ "$LAN_CIDR" =~ ^[0-9./]+(,\ [0-9./]+)*$ ]] || die "unexpected local network"
	cat > "$ETC/policy-up.sh" <<EOF
#!/bin/bash
set -euo pipefail
ip route replace default dev wg0 table 51820
EOF
	local range
	for range in ${LAN_CIDR//,/ }; do
		printf 'ip rule del priority %s 2>/dev/null || true\nip rule add to %s lookup main priority %s\n' "$prio" "$range" "$prio" >> "$ETC/policy-up.sh"
		prio=$((prio + 1))
	done
	for peer in "${SSH_PEERS[@]+"${SSH_PEERS[@]}"}"; do
		is_ipv4 "$peer" || die "unexpected SSH client address"
		printf 'ip rule del priority %s 2>/dev/null || true\nip rule add to %s/32 lookup main priority %s\n' "$prio" "$peer" "$prio" >> "$ETC/policy-up.sh"
		prio=$((prio + 1))
	done
	cat >> "$ETC/policy-up.sh" <<'EOF'
ip rule del priority 100 2>/dev/null || true
ip rule del priority 101 2>/dev/null || true
ip rule del priority 200 2>/dev/null || true
ip rule add fwmark 51820 lookup main priority 100
ip rule add fwmark 51821 lookup main priority 101
ip rule add lookup 51820 priority 200
EOF
	cat > "$ETC/policy-down.sh" <<'EOF'
#!/bin/bash
set +e
for prio in 100 101 200; do
	ip rule del priority "$prio" 2>/dev/null
done
for prio in $(seq 60 79); do
	ip rule del priority "$prio" 2>/dev/null
done
ip route flush table 51820 2>/dev/null
exit 0
EOF
	chmod 700 "$ETC/policy-up.sh" "$ETC/policy-down.sh"
}

write_ssh_routes() {
	local peer via="dev $PHYS"
	[[ "$PHYS" =~ ^[A-Za-z0-9._:-]+$ ]] || die "unexpected network interface name"
	if [[ -n "${ORIG_GW:-}" ]]; then
		is_ipv4 "$ORIG_GW" || die "unexpected default gateway"
		via="via $ORIG_GW dev $PHYS"
	fi
	: > "$ETC/ssh-up.sh"
	: > "$ETC/ssh-down.sh"
	printf '#!/bin/bash\nset -euo pipefail\n' >> "$ETC/ssh-up.sh"
	printf '#!/bin/bash\nset +e\n' >> "$ETC/ssh-down.sh"
	for peer in "${SSH_PEERS[@]+"${SSH_PEERS[@]}"}"; do
		is_ipv4 "$peer" || die "unexpected SSH client address"
		printf 'ip route replace %s/32 %s\n' "$peer" "$via" >> "$ETC/ssh-up.sh"
		printf 'ip route del %s/32 2>/dev/null\n' "$peer" >> "$ETC/ssh-down.sh"
	done
	printf 'exit 0\n' >> "$ETC/ssh-up.sh" "$ETC/ssh-down.sh"
	chmod 700 "$ETC/ssh-up.sh" "$ETC/ssh-down.sh"
}

write_nft() {
	local ssh_in="" ssh_out="" hop_rule="" addrs="" item
	local -a ssh_set=()
	[[ "$PHYS" =~ ^[A-Za-z0-9._:-]+$ ]] || die "unexpected network interface name"
	[[ "$LAN_CIDR" =~ ^[0-9./]+(,\ [0-9./]+)*$ ]] || die "unexpected local network"
	ssh_set+=("$LAN_CIDR")
	for item in "${SSH_PEERS[@]+"${SSH_PEERS[@]}"}"; do
		is_ipv4 "$item" || die "unexpected SSH client address"
		ssh_set+=("$item")
	done
	for item in "${ssh_set[@]}"; do
		if [[ -n "$addrs" ]]; then
			addrs+=", $item"
		else
			addrs="$item"
		fi
	done
	if [[ -n "$addrs" ]]; then
		ssh_in="tcp dport 22 ip saddr { $addrs } iifname \"$PHYS\" accept"
		ssh_out="tcp sport 22 ip daddr { $addrs } oifname \"$PHYS\" ct state established,related accept"
	fi
	if ((HOP1_ON)); then
		hop_rule="oifname \"hop1\" udp dport $VPN_PORT ip daddr $VPN_IP accept"
	fi
	# The assistant's model process may only answer on its own port. New connections it opens,
	# including to the bridge's own API on loopback, are dropped; its replies to the CLI are
	# established traffic and pass. The rule is keyed on the user, so it applies to nothing else.
	local assistant_rule=""
	if id bridgeassist >/dev/null 2>&1; then
		assistant_rule="meta skuid bridgeassist ct state new drop"
	fi
	cat > /etc/nftables.conf <<EOF
#!/usr/sbin/nft -f
flush ruleset
table inet ecash_bridge {
	chain input {
		type filter hook input priority 0; policy drop;
		iif "lo" accept
		ct state established,related accept
		$ssh_in
	}
	chain forward {
		type filter hook forward priority 0; policy drop;
	}
	chain output {
		type filter hook output priority 0; policy drop;
		$assistant_rule
		oif "lo" accept
		oifname "wg0" accept
		$hop_rule
		udp dport { 67, 68 } oifname != "wg0" oifname != "hop1" accept
		udp dport $OUTER_PORT ip daddr $OUTER_IP oifname != "wg0" oifname != "hop1" accept
		$ssh_out
	}
}
EOF
	chmod 600 /etc/nftables.conf
	nft -f /etc/nftables.conf
}

# The bundle is the only source of software. Every file is checked against MANIFEST before any of
# it is used, so a damaged copy or a file swapped on the stick is refused as a whole.
verify_bundle() {
	[[ -f "$BUNDLE/MANIFEST" && -f "$BUNDLE/BUNDLE_INFO" ]] || die "no bundle at $BUNDLE. Run ./make-bundle.sh on a machine with internet (Ubuntu 24.04, same architecture), then copy the repository with its bundle/ directory here."
	local arch expected
	arch="$(awk -F= '/^arch=/ {print $2}' "$BUNDLE/BUNDLE_INFO")"
	expected="$(dpkg --print-architecture)"
	[[ "$arch" == "$expected" ]] || die "the bundle was built for $arch; this machine is $expected"
	local ubuntu
	ubuntu="$(awk -F= '/^ubuntu=/ {print $2}' "$BUNDLE/BUNDLE_INFO")"
	[[ "$ubuntu" == 24.* ]] || die "the bundle was built on Ubuntu $ubuntu; this installer expects 24.04"
	(cd "$BUNDLE" && sha256sum --quiet -c MANIFEST) || die "bundle files do not match MANIFEST. Copy the bundle again."
	for f in swap-backend node.tar.xz cloudflared.deb frontend/build/index.js frontend/package.json debs/PACKAGES; do
		[[ -e "$BUNDLE/$f" ]] || die "bundle is missing $f"
	done
	BUNDLE_ASSISTANT=0
	if [[ "$(awk -F= '/^assistant=/ {print $2}' "$BUNDLE/BUNDLE_INFO")" == 1 && -f "$BUNDLE/llama-server" ]]; then
		BUNDLE_ASSISTANT=1
	fi
}

# Installs the bundled .deb files, skipping any the machine already has at the same or a newer
# version, so a bundle built on a slightly older image never downgrades anything. dpkg is used
# directly: given local files, apt still prefers to fetch the same versions from the archive its
# stale lists know about, and offline that fails. dpkg orders the set itself; a second pass and a
# configure pass pick up anything a pre-dependency made it postpone.
install_debs() {
	local deb pkg version installed
	local -a wanted=()
	for deb in "$BUNDLE"/debs/*.deb; do
		pkg="$(dpkg-deb -f "$deb" Package)"
		version="$(dpkg-deb -f "$deb" Version)"
		installed="$(dpkg-query -W -f='${db:Status-Status} ${Version}' "$pkg" 2>/dev/null || true)"
		if [[ "$installed" == installed* ]]; then
			dpkg --compare-versions "${installed#installed }" ge "$version" && continue
		fi
		wanted+=("$deb")
	done
	if ((${#wanted[@]})); then
		printf 'Installing %s package files from the bundle\n' "${#wanted[@]}"
		dpkg -i "${wanted[@]}" >/dev/null 2>&1 || dpkg -i "${wanted[@]}" >/dev/null 2>&1 || true
		dpkg --configure -a
	fi
	# The packages the server needs, by name, must now all be present whatever the image had.
	while read -r pkg; do
		[[ -n "$pkg" ]] || continue
		dpkg-query -W -f='${db:Status-Status}' "$pkg" 2>/dev/null | grep -q '^installed$' || die "package $pkg is not installed after the bundle; the bundle was built against a different image"
	done < "$BUNDLE/debs/PACKAGES"
}

install_node() {
	local sha
	case "$(uname -m)" in
		x86_64) sha="$NODE_SHA_X64" ;;
		aarch64) sha="$NODE_SHA_ARM64" ;;
		*) die "unsupported architecture: $(uname -m)" ;;
	esac
	printf '%s  %s\n' "$sha" "$BUNDLE/node.tar.xz" | sha256sum -c - >/dev/null || die "bundled Node tarball does not match the pinned hash"
	if [[ -x /usr/local/bin/node && "$(/usr/local/bin/node --version)" == "$NODE_VERSION" ]]; then
		return
	fi
	tar -xJf "$BUNDLE/node.tar.xz" -C /usr/local --strip-components=1
	/usr/local/bin/node --version >/dev/null
}

install_cloudflared() {
	local version installed
	version="$(dpkg-deb -f "$BUNDLE/cloudflared.deb" Version)"
	installed="$(dpkg-query -W -f='${db:Status-Status} ${Version}' cloudflared 2>/dev/null || true)"
	if [[ "$installed" == installed* ]] && dpkg --compare-versions "${installed#installed }" ge "$version"; then
		return
	fi
	apt-get install -y --no-install-recommends "$BUNDLE/cloudflared.deb"
}

require_ubuntu() {
	[[ -r /etc/os-release ]] || die "this installer is for Ubuntu Server 24.04"
	# shellcheck disable=SC1091
	. /etc/os-release
	[[ "${ID:-}" == "ubuntu" ]] || die "this installer is for Ubuntu, found ${ID:-unknown}"
	[[ "${VERSION_ID:-0}" == 24.* || "${VERSION_ID:-0}" > 24 ]] || die "Ubuntu 24.04 or newer is required, found ${VERSION_ID:-unknown}"
}

copy_tree() {
	# umask 077 would make every directory here root-only. The service users need to traverse to
	# their working directories and the backend binary, so those two get explicit modes. The
	# source copy stays root-only, and is owned by root rather than by whoever owned the checkout.
	install -d -m 755 "$OPT" "$OPT/backend"
	install -d -m 700 "$OPT/src"
	mkdir -p "$OPT/backend/data" "$OPT/frontend"
	local rsync_bin="rsync"
	command -v rsync >/dev/null || rsync_bin=""
	if [[ -n "$rsync_bin" ]]; then
		rsync -a --no-owner --no-group --delete \
			--exclude params.conf \
			--exclude backend/.env \
			--exclude backend/target \
			--exclude backend/data \
			--exclude frontend/node_modules \
			--exclude frontend/build \
			--exclude frontend/.svelte-kit \
			"$ROOT/" "$OPT/src/"
	else
		tar -C "$ROOT" \
			--exclude params.conf \
			--exclude backend/.env \
			--exclude backend/target \
			--exclude backend/data \
			--exclude frontend/node_modules \
			--exclude frontend/build \
			-cf - . | tar -C "$OPT/src" --no-same-owner -xf -
	fi
}

run_as_backend() {
	systemd-run --quiet --pipe --wait --collect \
		--uid=bridgebackend --gid=bridgebackend \
		--working-directory="$OPT/backend" \
		--property=EnvironmentFile="${RUN_ENV:-$ETC/backend.env}" \
		"$OPT/backend/swap-backend" "$@"
}

# A re-install stops the bridge services before the wallet checks, which need the wallet-store
# lock. If the install is refused after that point, the uncommitted env file is discarded and the
# services are started again on the configuration they had, so the bridge is not left down.
rollback_services() {
	ROLLBACK_SERVICES=0
	rm -f "$ETC/backend.env.new"
	if ((SERVICES_WERE_ACTIVE)); then
		systemctl start ecash-bridge-backend.service ecash-bridge-frontend.service 2>/dev/null || true
		printf 'install.sh: nothing was changed. The services were started again on their previous configuration.\n' >&2
	fi
}

# Waits for a freshly started unit to answer on its local port. `systemctl enable --now` returns
# as soon as the process is forked, so a backend that dies on a configuration error would
# otherwise be reported as installed while it restarts in a loop.
wait_for_service() {
	local unit="$1" url="$2" attempt
	for attempt in $(seq 1 60); do
		systemctl is-active --quiet "$unit" || die "$unit stopped right after starting. Run: journalctl -u $unit -n 50"
		if curl -fsS --max-time 5 "$url" >/dev/null 2>&1; then
			return 0
		fi
		sleep 2
	done
	die "$unit did not answer on $url within two minutes. Run: journalctl -u $unit -n 50"
}

cmd_check() {
	local file="$1"
	parse_params "$file"
	validate_params
	printf 'params.conf is complete. Secrets were not printed.\n'
	if ((HOP1_ON)); then
		printf 'Hop 1 is set. The installer will route hop 2 (%s) through hop 1 (%s).\n' "${VPN_PROVIDER:-the exit VPN}" "${HOP1_PROVIDER:-the inner VPN}"
	else
		printf 'Hop 1 is empty. %s will be the only VPN.\n' "${VPN_PROVIDER:-The exit VPN}"
	fi
}

# The local assistant: llama-server from the bundle, the pinned model, the runbook, and the CLI.
# The model process gets its own user with read access to nothing but its own directory, and a
# unit that denies it every network address except loopback. Without a model file the pieces are
# still installed and the CLI explains what is missing.
install_assistant() {
	# shellcheck disable=SC1091
	. "$ROOT/assistant/MODEL.conf"
	install -d -m 755 "$ASSIST"
	install -m 644 "$ROOT/assistant/RUNBOOK.md" "$ASSIST/RUNBOOK.md"
	install -m 644 "$ROOT/assistant/MODEL-LICENSE.md" "$ASSIST/MODEL-LICENSE.md"
	install -m 755 "$ROOT/assistant/bridge-assistant" /usr/local/sbin/bridge-assistant
	if ((!BUNDLE_ASSISTANT)); then
		printf 'The bundle has no assistant; bridge-assistant is installed but has no model to run.\n'
		return
	fi
	id bridgeassist >/dev/null 2>&1 || useradd --system --home-dir "$ASSIST" --shell /usr/sbin/nologin bridgeassist
	install -m 755 "$BUNDLE/llama-server" "$ASSIST/llama-server"
	if [[ -f "$BUNDLE/model/$MODEL_FILE" ]]; then
		# Already verified against MANIFEST; this is the pin the repository carries.
		printf '%s  %s\n' "$MODEL_SHA256" "$BUNDLE/model/$MODEL_FILE" | sha256sum -c - >/dev/null || die "the bundled model does not match assistant/MODEL.conf"
		if [[ ! -f "$ASSIST/model.gguf" ]] || ! cmp -s "$BUNDLE/model/$MODEL_FILE" "$ASSIST/model.gguf"; then
			cp "$BUNDLE/model/$MODEL_FILE" "$ASSIST/model.gguf.tmp"
			mv -f "$ASSIST/model.gguf.tmp" "$ASSIST/model.gguf"
		fi
		chmod 644 "$ASSIST/model.gguf"
	else
		printf 'The bundle has no model file; the assistant cannot answer until one is added.\n'
	fi
	local threads
	threads="$(nproc)"
	((threads > 8)) && threads=8
	sed "s/--threads 4/--threads $threads/" "$ROOT/assistant/ecash-bridge-assistant.service" > /etc/systemd/system/ecash-bridge-assistant.service
	chmod 644 /etc/systemd/system/ecash-bridge-assistant.service
	systemctl daemon-reload
	systemctl disable ecash-bridge-assistant.service >/dev/null 2>&1 || true
}

# What the machine's traffic leaves from, via the tunnel. Empty when nothing answers.
egress_ip() {
	curl --max-time 15 -fsS https://ipv4.icanhazip.com 2>/dev/null | tr -d '[:space:]' || true
}

# One screen of operating state, with nothing in it that must stay secret: no keys, no mnemonic,
# no customer addresses. It is meant to be read by the operator, pasted to anyone helping them, or
# handed to the local assistant.
cmd_status() {
	[[ "$(id -u)" -eq 0 ]] || die "status must be run as root"
	[[ -f "$ETC/backend.env" ]] || die "the bridge is not installed here"
	# A report, not an installation: probes that fail are findings, not errors to abort on.
	trap - ERR
	set +e
	local expected_ip hostname unit hop1=0
	expected_ip="$(awk -F'"' '/^VPN_PUBLIC_IPV4 *=/ {print $2}' "$ETC/params.conf" 2>/dev/null)"
	hostname="$(awk -F= '/^CLOUDFLARE_HOSTNAME=/ {print $2}' "$ETC/public.env" 2>/dev/null)"
	[[ -f /etc/wireguard/hop1.conf ]] && hop1=1

	printf 'ECX bridge status  %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
	printf '\nServices\n'
	for unit in nftables wg-quick@hop1 wg-quick@wg0 ecash-bridge-backend ecash-bridge-frontend ecash-bridge-cloudflared; do
		[[ "$unit" == wg-quick@hop1 && $hop1 -eq 0 ]] && continue
		printf '  %-28s %s\n' "$unit" "$(systemctl is-active "$unit" 2>/dev/null || true)"
	done

	printf '\nVPN tunnel (the WireGuard tunnel all traffic leaves through; not the Cloudflare tunnel)\n'
	local iface hs
	for iface in hop1 wg0; do
		[[ "$iface" == hop1 && $hop1 -eq 0 ]] && continue
		if wg show "$iface" >/dev/null 2>&1; then
			hs="$(wg show "$iface" latest-handshakes | awk '{print $2}')"
			if [[ -n "$hs" && "$hs" != 0 ]]; then
				printf '  %-6s handshake %s seconds ago\n' "$iface" "$(( $(date +%s) - hs ))"
			else
				printf '  %-6s no handshake yet\n' "$iface"
			fi
		else
			printf '  %-6s down\n' "$iface"
		fi
	done
	local egress
	egress="$(egress_ip)"
	if [[ -z "$egress" ]]; then
		printf '  egress    unreachable (no internet through the tunnel)\n'
	elif [[ "$egress" == "$expected_ip" ]]; then
		printf '  egress    %s (matches VPN_PUBLIC_IPV4)\n' "$egress"
	else
		printf '  egress    %s  !! does not match VPN_PUBLIC_IPV4 %s\n' "$egress" "$expected_ip"
	fi

	if grep -qx 'ALLOW_LIVE_PAYOUTS=1' "$ETC/backend.env"; then
		printf '\nPayouts   on (enable has been run)\n'
		[[ -n "$hostname" ]] && printf 'Public    https://%s, served through the Cloudflare tunnel, which should be active\n' "$hostname"
	else
		printf '\nPayouts   off: enable has not been run yet. Expected on a new install. The Cloudflare tunnel is\n'
		printf '          meant to be inactive in this state; the site is not public yet. Deposit both treasuries,\n'
		printf '          then run enable. Nothing is sent to anyone while payouts are off.\n'
	fi

	printf '\nTreasury and orders\n'
	local stats
	stats="$(curl --max-time 25 -fsS http://127.0.0.1:3000/stats 2>/dev/null || true)"
	if [[ -n "$stats" ]]; then
		python3 "$OPT/src/assistant/status-stats.py" "$stats" || printf '  (could not read /stats)\n'
	else
		printf '  backend not answering on 127.0.0.1:3000\n'
	fi
	if [[ -f "$OPT/backend/data/bridge.db" ]]; then
		python3 "$OPT/src/assistant/status-orders.py" "$OPT/backend/data/bridge.db" 2>/dev/null || true
	fi

	printf '\nRecent backend errors (last 10)\n'
	journalctl -u ecash-bridge-backend -n 300 --no-pager -o short-iso 2>/dev/null \
		| grep -iE 'error|failed|denied|unknown outcome|insufficient|mismatch|\[unmatched\]|lost contact|panicked' \
		| tail -n 10 | sed 's/^/  /' || true
	local tick
	tick="$(journalctl -u ecash-bridge-backend -n 300 --no-pager -o short-iso 2>/dev/null | grep '\[poll\] tick' | tail -n 1 | cut -d' ' -f1 || true)"
	[[ -n "$tick" ]] && printf '  last poll tick %s\n' "$tick"

	printf '\nMachine\n'
	printf '  disk free  %s on /\n' "$(df -h / | awk 'NR==2 {print $4}')"
	printf '  memory     %s available of %s\n' "$(free -h | awk '/Mem/ {print $7}')" "$(free -h | awk '/Mem/ {print $2}')"
	if [[ -x /usr/lib/update-notifier/apt-check ]]; then
		local updates
		updates="$(/usr/lib/update-notifier/apt-check 2>&1 || true)"
		printf '  updates    %s pending, %s security\n' "${updates%%;*}" "${updates##*;}"
	fi
	[[ -f /var/run/reboot-required ]] && printf '  reboot     required for a pending update\n'
	printf '  uptime     %s\n' "$(uptime -p)"
	if [[ -f "$ASSIST/model.gguf" ]]; then
		printf '  assistant  installed; run: sudo bridge-assistant\n'
	fi
	return 0
}

cmd_addresses() {
	[[ "$(id -u)" -eq 0 ]] || die "addresses must be run as root"
	[[ -f /var/lib/ecash-bridge/deposit-addresses.txt ]] || die "no saved addresses yet. Run the install first."
	cat /var/lib/ecash-bridge/deposit-addresses.txt
}

cmd_enable() {
	[[ "$(id -u)" -eq 0 ]] || die "enable must be run as root"
	[[ "${1:-}" == "--i-deposited" ]] || die "refusing to enable payouts without --i-deposited"
	[[ -f "$ETC/backend.env" ]] || die "backend env is missing. Run the install first."
	wg show wg0 >/dev/null 2>&1 || die "wg0 is not up. Do not enable payouts without the VPN."
	local expected_ip egress
	expected_ip="$(awk -F'"' '/^VPN_PUBLIC_IPV4 *=/ {print $2}' "$ETC/params.conf")"
	egress="$(egress_ip)"
	[[ -n "$egress" ]] || die "nothing answers through the tunnel. Payouts stay off until the VPN carries traffic; run status."
	[[ "$egress" == "$expected_ip" ]] || die "public address is $egress, not VPN_PUBLIC_IPV4 ($expected_ip). Payouts stay off until traffic leaves through the VPN."
	python3 - <<PY
from pathlib import Path
path = Path("$ETC/backend.env")
text = path.read_text()
old = "ALLOW_LIVE_PAYOUTS=0"
new = "ALLOW_LIVE_PAYOUTS=1"
if old not in text and new not in text:
    raise SystemExit("ALLOW_LIVE_PAYOUTS is missing from backend.env")
path.write_text(text.replace(old, new))
path.chmod(0o600)
PY
	install -d /etc/systemd/system/wg-quick@wg0.service.d
	printf '[Unit]\nWants=ecash-bridge-cloudflared.service\n' > /etc/systemd/system/wg-quick@wg0.service.d/cloudflared.conf
	systemctl daemon-reload
	systemctl enable ecash-bridge-cloudflared.service
	systemctl restart ecash-bridge-cloudflared.service
	systemctl restart ecash-bridge-backend.service
	wait_for_service ecash-bridge-backend.service http://127.0.0.1:3000/quote
	printf 'Payouts are on and the Cloudflare tunnel is started.\n'
	printf 'Customers use https://%s once the connector shows connected.\n' "$(awk -F= '/^CLOUDFLARE_HOSTNAME=/{print $2}' "$ETC/public.env")"
}

cmd_install() {
	local file="$1"
	[[ "$(id -u)" -eq 0 ]] || die "install must be run as root"
	require_ubuntu
	parse_params "$file"
	validate_params
	verify_bundle
	# A re-install over a bridge whose operator already ran `enable` keeps payouts and the public
	# tunnel on. Only a first install leaves them off. The deposit attestation is not asked twice.
	WAS_LIVE=0
	if [[ -f "$ETC/backend.env" ]] && grep -qx 'ALLOW_LIVE_PAYOUTS=1' "$ETC/backend.env"; then
		WAS_LIVE=1
	fi
	# A re-install must keep the wallets and the mint the order database was written against. The
	# addresses saved by the previous run identify the wallets; the mint comes from the previous
	# env file. The wallet checks need the new binary and run after the build; the mint check is
	# cheap and runs now.
	SAVED_ECX_ADDR=""
	SAVED_SOL_ADDR=""
	if [[ -f /var/lib/ecash-bridge/deposit-addresses.txt ]]; then
		SAVED_ECX_ADDR="$(awk -F': ' '/^ECX deposit address:/ {print $2}' /var/lib/ecash-bridge/deposit-addresses.txt)"
		SAVED_SOL_ADDR="$(awk -F': ' '/^Solana treasury:/ {print $2}' /var/lib/ecash-bridge/deposit-addresses.txt)"
	fi
	if [[ -f "$ETC/backend.env" ]]; then
		local previous_mint
		previous_mint="$(awk -F'"' '/^WECX_MINT_ADDRESS=/ {print $2}' "$ETC/backend.env")"
		[[ -z "$previous_mint" || "$previous_mint" == "$WECX_MINT_ADDRESS" ]] || die "WECX_MINT_ADDRESS is not the mint this bridge was installed with ($previous_mint). The order database counts tokens of that mint. $NEW_BRIDGE_HINT"
	fi

	local vpn_host vpn_port hop_host hop_port
	read -r vpn_host vpn_port < <(split_endpoint "$VPN_WG_ENDPOINT")
	VPN_IP="$(resolve_v4 "$vpn_host")"
	VPN_PORT="$vpn_port"
	if ((HOP1_ON)); then
		read -r hop_host hop_port < <(split_endpoint "$HOP1_WG_ENDPOINT")
		HOP1_IP="$(resolve_v4 "$hop_host")"
		HOP1_PORT="$hop_port"
		OUTER_IP="$HOP1_IP"
		OUTER_PORT="$HOP1_PORT"
	else
		OUTER_IP="$VPN_IP"
		OUTER_PORT="$VPN_PORT"
	fi

	# The machine may have no network yet: an offline install from the bundle, with the cable
	# plugged in afterwards. Then the physical interface is the first one that is not loopback or a
	# tunnel, and the local network is unknown, so SSH is admitted from any private range.
	ONLINE=1
	PHYS="$(ip -4 route show default | awk '{print $5; exit}')"
	if [[ -n "$PHYS" ]]; then
		ORIG_GW="$(ip -4 route show default | awk '{print $3; exit}')"
		LAN_CIDR="$(ip -4 route show dev "$PHYS" proto kernel | awk '{print $1; exit}')"
		[[ "$LAN_CIDR" =~ ^[0-9.]+/[0-9]+$ ]] || die "could not find the IPv4 subnet on $PHYS"
	else
		ONLINE=0
		ORIG_GW=""
		PHYS="$(ip -o link show | awk -F': ' '$2 != "lo" && $2 !~ /^(wg|hop|docker|veth|br-|virbr)/ {print $2; exit}' | cut -d@ -f1)"
		[[ -n "$PHYS" ]] || die "no network interface found"
		LAN_CIDR="10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16"
		printf 'No network yet. Installing for a first connection through the VPN; SSH will be accepted from private addresses.\n'
	fi

	export DEBIAN_FRONTEND=noninteractive
	install_debs
	install_node

	# Created before the firewall is written: its rule for the assistant's user needs the uid.
	if ((BUNDLE_ASSISTANT)); then
		id bridgeassist >/dev/null 2>&1 || useradd --system --home-dir "$ASSIST" --shell /usr/sbin/nologin bridgeassist
	fi
	install -d -m 711 "$ETC"
	install -d -m 700 /var/lib/ecash-bridge /etc/wireguard
	install -m 600 "$file" "$ETC/params.conf"
	printf 'CLOUDFLARE_HOSTNAME=%s\n' "$CLOUDFLARE_HOSTNAME" > "$ETC/public.env"
	chmod 644 "$ETC/public.env"
	collect_ssh_peers

	sysctl -w net.ipv6.conf.all.disable_ipv6=1
	sysctl -w net.ipv6.conf.default.disable_ipv6=1
	printf 'net.ipv6.conf.all.disable_ipv6=1\nnet.ipv6.conf.default.disable_ipv6=1\n' > /etc/sysctl.d/99-ecash-bridge.conf

	# Bring any existing tunnels down with the configuration they were brought up with, so their
	# PreDown hooks and wg-quick's own routing rules are removed. Writing the new files first left
	# the old rules behind, and in a switch from two hops to one the hop1 interface stayed up with
	# its route to the hop 2 endpoint, so wg0 could never handshake again until a reboot. The bridge
	# services stop with wg0 and are started again at the end of the install.
	systemctl stop wg-quick@wg0.service wg-quick@hop1.service 2>/dev/null || true

	if ((HOP1_ON)); then
		write_policy_scripts
		write_wg hop1 "$HOP1_WG_PRIVATE_KEY" "$HOP1_WG_ADDRESS" "$HOP1_WG_PEER_PUBLIC_KEY" "$HOP1_IP" "$HOP1_PORT" "$(cat <<EOF
Table = off
FwMark = 51821
PostUp = ip route replace ${VPN_IP}/32 dev %i
PreDown = ip route del ${VPN_IP}/32 dev %i || true
EOF
)"
		write_wg wg0 "$VPN_WG_PRIVATE_KEY" "$VPN_WG_ADDRESS" "$VPN_WG_PEER_PUBLIC_KEY" "$VPN_IP" "$VPN_PORT" "$(cat <<EOF
Table = off
FwMark = 51820
MTU = 1280
DNS = $VPN_WG_DNS
PostUp = $ETC/policy-up.sh
PreDown = $ETC/policy-down.sh
EOF
)"
		install -d /etc/systemd/system/wg-quick@wg0.service.d /etc/systemd/system/wg-quick@hop1.service.d
		printf '[Unit]\nAfter=nftables.service wg-quick@hop1.service\nWants=nftables.service\nRequires=wg-quick@hop1.service\nPartOf=wg-quick@hop1.service\n' > /etc/systemd/system/wg-quick@wg0.service.d/hop1.conf
		# wg0 is PartOf hop1, which only propagates stop and restart; Wants makes a start of hop1
		# bring wg0, and through wg0's own drop-in the bridge, back up.
		printf '[Unit]\nAfter=nftables.service\nWants=nftables.service wg-quick@wg0.service\n' > /etc/systemd/system/wg-quick@hop1.service.d/firewall.conf
	else
		write_ssh_routes
		write_wg wg0 "$VPN_WG_PRIVATE_KEY" "$VPN_WG_ADDRESS" "$VPN_WG_PEER_PUBLIC_KEY" "$VPN_IP" "$VPN_PORT" "$(cat <<EOF
DNS = $VPN_WG_DNS
PostUp = $ETC/ssh-up.sh
PreDown = $ETC/ssh-down.sh
EOF
)"
		systemctl disable wg-quick@hop1.service >/dev/null 2>&1 || true
		rm -f /etc/systemd/system/wg-quick@wg0.service.d/hop1.conf /etc/wireguard/hop1.conf /etc/systemd/system/wg-quick@hop1.service.d/firewall.conf
		install -d /etc/systemd/system/wg-quick@wg0.service.d
		printf '[Unit]\nAfter=nftables.service\nWants=nftables.service\n' > /etc/systemd/system/wg-quick@wg0.service.d/firewall.conf
	fi
	# Starting the tunnel also starts the bridge. The PartOf lines in the service drop-ins only
	# propagate stop and restart, so a plain stop-then-start of wg0 left the bridge down. This takes
	# effect at the daemon-reload further down, after the units exist.
	printf '[Unit]\nWants=ecash-bridge-backend.service ecash-bridge-frontend.service\n' > /etc/systemd/system/wg-quick@wg0.service.d/bridge.conf
	if ((WAS_LIVE)); then
		printf '[Unit]\nWants=ecash-bridge-cloudflared.service\n' > /etc/systemd/system/wg-quick@wg0.service.d/cloudflared.conf
	else
		rm -f /etc/systemd/system/wg-quick@wg0.service.d/cloudflared.conf
	fi
	write_nft
	systemctl enable nftables.service
	if ((HOP1_ON)); then
		systemctl enable wg-quick@hop1.service
		systemctl restart wg-quick@hop1.service
	fi
	systemctl enable wg-quick@wg0.service
	systemctl restart wg-quick@wg0.service
	local handshake=0 attempt
	for attempt in $(seq 1 20); do
		if wg show wg0 | awk '/latest handshake/ {found=1} END {exit !found}'; then
			handshake=1
			break
		fi
		sleep 1
	done
	unset attempt
	if ((handshake)); then
		local egress
		egress="$(curl --max-time 20 -fsS https://ipv4.icanhazip.com | tr -d '[:space:]' || true)"
		[[ "$egress" == "$VPN_PUBLIC_IPV4" ]] || die "public address is ${egress:-unknown}, not VPN_PUBLIC_IPV4. The firewall is on. SSH from the local network, or the session that started this install, still works. Fix the VPN fields and run the installer again."
	elif ((ONLINE)); then
		die "wg0 has no handshake. The firewall is on. SSH from the local network, or the session that started this install, still works. Fix the VPN fields and run the installer again."
	else
		printf 'The tunnel is configured and will connect when the network is.\n'
	fi

	install_cloudflared
	systemctl disable --now cloudflared.service >/dev/null 2>&1 || true

	copy_tree
	install -m 755 "$BUNDLE/swap-backend" "$OPT/backend/swap-backend"
	# On a re-install both services may be running. The frontend must not keep serving a tree
	# that is being replaced, and the backend holds the wallet-store lock the derivation commands
	# below need.
	SERVICES_WERE_ACTIVE=0
	systemctl is-active --quiet ecash-bridge-backend.service && SERVICES_WERE_ACTIVE=1
	systemctl stop ecash-bridge-frontend.service ecash-bridge-backend.service 2>/dev/null || true
	# From here until the new env file is committed, a refusal starts the previous configuration
	# again (see rollback_services), so a rejected upgrade does not leave a live bridge down.
	ROLLBACK_SERVICES=1
	rm -rf "$OPT/frontend"
	mkdir -p "$OPT/frontend"
	cp -a "$BUNDLE/frontend/build" "$BUNDLE/frontend/package.json" "$BUNDLE/frontend/node_modules" "$OPT/frontend/"

	id bridgebackend >/dev/null 2>&1 || useradd --system --home-dir "$OPT/backend" --shell /usr/sbin/nologin bridgebackend
	id bridgeweb >/dev/null 2>&1 || useradd --system --home-dir "$OPT/frontend" --shell /usr/sbin/nologin bridgeweb
	id cloudflared >/dev/null 2>&1 || useradd --system --home-dir /var/lib/ecash-bridge --shell /usr/sbin/nologin cloudflared
	install -d -m 700 -o bridgebackend -g bridgebackend "$OPT/backend/data"
	chown -R bridgeweb:bridgeweb "$OPT/frontend"
	chown root:root "$OPT/backend/swap-backend"

	# The new environment is written beside the old one and only moved into place once the wallet
	# checks below have passed against it. Until then the previous env file stays authoritative.
	write_env_file "$ETC/backend.env.new"
	RUN_ENV="$ETC/backend.env.new"

	install -m 644 "$ROOT/deploy/ecash-bridge-backend.service" /etc/systemd/system/ecash-bridge-backend.service
	install -m 644 "$ROOT/deploy/ecash-bridge-frontend.service" /etc/systemd/system/ecash-bridge-frontend.service
	install -m 644 "$ROOT/deploy/edge/cloudflared.service" /etc/systemd/system/ecash-bridge-cloudflared.service
	install -d /etc/systemd/system/ecash-bridge-backend.service.d /etc/systemd/system/ecash-bridge-frontend.service.d /etc/systemd/system/ecash-bridge-cloudflared.service.d
	printf '[Unit]\nAfter=nftables.service wg-quick@wg0.service\nRequires=wg-quick@wg0.service\nPartOf=wg-quick@wg0.service\n' > /etc/systemd/system/ecash-bridge-backend.service.d/vpn.conf
	printf '[Unit]\nAfter=nftables.service wg-quick@wg0.service\nRequires=wg-quick@wg0.service\nPartOf=wg-quick@wg0.service\n[Service]\nEnvironmentFile=%s\n' "$ETC/frontend.env" > /etc/systemd/system/ecash-bridge-frontend.service.d/vpn.conf
	printf '[Unit]\nAfter=nftables.service wg-quick@wg0.service\nRequires=wg-quick@wg0.service\nPartOf=wg-quick@wg0.service\n' > /etc/systemd/system/ecash-bridge-cloudflared.service.d/vpn.conf
	systemctl daemon-reload

	local ecx_addr sol_addr owned
	sol_addr="$(run_as_backend solana-pubkey | awk '{print $NF}')"
	[[ -n "$sol_addr" ]] || die "could not derive the Solana treasury address"
	if [[ -n "$SAVED_SOL_ADDR" && "$SAVED_SOL_ADDR" != "$sol_addr" ]]; then
		die "SOLANA_KEYPAIR is not the key this bridge was installed with (its treasury is $SAVED_SOL_ADDR). The order database and the treasury funds belong to that key. $NEW_BRIDGE_HINT"
	fi
	if [[ -n "$SAVED_ECX_ADDR" ]]; then
		# Keep the address printed by the first install. Re-deriving "the next unused address"
		# once orders exist can return an abandoned order's deposit address, and float sent there
		# would be paid out to that order's customer. Checking that the wallet still owns the saved
		# address is also the mnemonic and passphrase consistency check. A changed mnemonic usually
		# fails one step earlier: the wallet store refuses descriptors it was not created with.
		local check
		check="$(run_as_backend owns-address --address "$SAVED_ECX_ADDR" 2>&1)" || true
		case "$(awk '/^\[ecx\] owns/ {print $NF}' <<<"$check")" in
			true) ;;
			false) die "WALLET_MNEMONIC or WALLET_PASSPHRASE is not the wallet this bridge was installed with (its deposit address is $SAVED_ECX_ADDR). The order database and the deposits belong to that wallet. $NEW_BRIDGE_HINT" ;;
			*) die "the hot wallet store at $OPT/backend/data/ecx.db could not be opened with these keys: $(tail -n 1 <<<"$check"). If WALLET_MNEMONIC or WALLET_PASSPHRASE changed, that store belongs to the previous wallet. $NEW_BRIDGE_HINT" ;;
		esac
		ecx_addr="$SAVED_ECX_ADDR"
	else
		ecx_addr="$(run_as_backend address | awk '/\[ecx\]/ {print $2}')"
		[[ "$ecx_addr" == bc1q* ]] || die "could not derive the ECX deposit address"
	fi

	# The owner addresses must not be the hot wallets: a sweep to the hot wallet would record fees
	# as moved to cold storage while they stayed on this machine. The backend refuses to start in
	# that case too; checking here gives the message before anything is enabled.
	[[ "$sol_addr" != "$OWNER_SOL_ADDRESS" ]] || die "OWNER_SOL_ADDRESS is the Solana treasury itself. Use a wallet you control on a different key."
	owned="$(run_as_backend owns-address --address "$OWNER_ECX_ADDRESS" | awk '{print $NF}')" || owned=""
	case "$owned" in
		false) ;;
		true) die "OWNER_ECX_ADDRESS belongs to the hot wallet. Use a wallet you control on a different seed." ;;
		*) die "could not check OWNER_ECX_ADDRESS against the hot wallet" ;;
	esac

	# Every check has passed: commit the new environment and the rest of the configuration.
	mv -f "$ETC/backend.env.new" "$ETC/backend.env"
	RUN_ENV=""
	ROLLBACK_SERVICES=0
	write_earnings
	printf 'PUBLIC_WECX_MINT=%s\nPUBLIC_ASSET_TICKER=%s\n' "$WECX_MINT_ADDRESS" "${ASSET_TICKER:-bECX}" > "$ETC/frontend.env"
	chmod 644 "$ETC/frontend.env"
	printf 'TUNNEL_TOKEN=%s\n' "$CLOUDFLARE_TUNNEL_TOKEN" > "$ETC/cloudflared.env"
	chown root:cloudflared "$ETC/cloudflared.env"
	chmod 640 "$ETC/cloudflared.env"

	cat > /var/lib/ecash-bridge/deposit-addresses.txt <<EOF
ECX deposit address: $ecx_addr
Solana treasury: $sol_addr
EOF
	chmod 644 /var/lib/ecash-bridge/deposit-addresses.txt

	systemctl enable ecash-bridge-backend.service ecash-bridge-frontend.service
	# restart, not enable --now: the latter leaves an already-running service on its old binary
	# or build, which is how a re-install once left Node serving a manifest for chunks that no
	# longer existed.
	install_assistant
	systemctl restart ecash-bridge-backend.service ecash-bridge-frontend.service
	if ((handshake)); then
		wait_for_service ecash-bridge-backend.service http://127.0.0.1:3000/quote
		wait_for_service ecash-bridge-frontend.service http://127.0.0.1:3001/
	fi
	# Without a handshake the backend cannot reach the chain RPC and exits; its unit restarts it
	# every ten seconds without limit, so it comes up by itself once the tunnel carries traffic.
	# wg-quick itself succeeds offline (it only creates the interface), so there is no systemd
	# event to hang a later start on.

	if ((WAS_LIVE)); then
		systemctl enable ecash-bridge-cloudflared.service
		((handshake)) && systemctl restart ecash-bridge-cloudflared.service
		printf '\nReinstalled. Payouts stay on and the Cloudflare tunnel is running, as before this run.\n'
		cat /var/lib/ecash-bridge/deposit-addresses.txt
		return
	fi
	systemctl disable --now ecash-bridge-cloudflared.service >/dev/null 2>&1 || true

	printf '\nInstalled. Payouts are off and the public tunnel is not started.\n'
	cat /var/lib/ecash-bridge/deposit-addresses.txt
	if ((!handshake)); then
		printf '\nConnect the network. The firewall lets out only the VPN handshake; the tunnel and the services start on their own.\n'
		printf 'Then run: sudo %s status\n' "$ROOT/install.sh"
	fi
	printf '\nDeposit %s to the bc1q address. Deposit SOL and w%s to the Solana treasury.\n' "${ASSET_TICKER:-bECX}" "${ASSET_TICKER:-bECX}"
	printf 'Then run: sudo %s enable --i-deposited\n' "$ROOT/install.sh"
}

main() {
	WALLET_MNEMONIC=""
	WALLET_PASSPHRASE=""
	SOLANA_KEYPAIR=""
	WECX_MINT_ADDRESS=""
	ASSET_TICKER=""
	OWNER_ECX_ADDRESS=""
	OWNER_SOL_ADDRESS=""
	case "${1:-}" in
		-h|--help|help) usage ;;
		check) [[ -n "${2:-}" ]] || die "check needs the params file"; cmd_check "$2" ;;
		addresses) cmd_addresses ;;
		status) cmd_status ;;
		enable) cmd_enable "${2:-}" ;;
		"") usage; die "a params file is required" ;;
		*) cmd_install "$1" ;;
	esac
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
	main "$@"
fi
