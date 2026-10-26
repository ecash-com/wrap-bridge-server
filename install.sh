#!/bin/bash
# Install the ECX bridge on a dedicated Ubuntu Server.
# Prepare first: docs/prepare.md. This script does not create wallets.
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ETC="/etc/ecash-bridge"
OPT="/opt/ecash-bridge"
NODE_VERSION="v22.23.4"
NODE_SHA_X64="1805bcd35e6815559bd2eb97db438d3fdb0eb3a5e6a977b7c1badca76337e9ce"
NODE_SHA_ARM64="6287aa46b6c375d816e63ae66d024fbbfd70404825aabdfdf898a970871d2f4b"

die() { printf 'install.sh: %s\n' "$1" >&2; exit 1; }

usage() {
	cat <<'EOF'
Usage:
  ./install.sh check /path/to/params.conf
  sudo ./install.sh /path/to/params.conf
  sudo ./install.sh addresses
  sudo ./install.sh enable --i-deposited

check does not install and does not print secrets.
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

parse_params() {
	local file="$1" line key value
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
			PEGIN_FEE_BPS|PEGOUT_FEE_BPS|PEGIN_FLAT_FEE_SAT|MIN_DEPOSIT_AMOUNT_SAT|ECX_DEPOSIT_MIN_CONFS|\
			SOL_STIPEND_LAMPORTS|SOL_STIPEND_PRICE_SAT|SOL_STIPEND_USD_LABEL|\
			ECX_ESPLORA_URL|SOLANA_RPC_URL|\
			VPN_PROVIDER|VPN_WG_PRIVATE_KEY|VPN_WG_ADDRESS|VPN_WG_DNS|VPN_WG_PEER_PUBLIC_KEY|VPN_WG_ENDPOINT|VPN_WG_ALLOWED_IPS|VPN_PUBLIC_IPV4|\
			HOP1_PROVIDER|HOP1_WG_PRIVATE_KEY|HOP1_WG_ADDRESS|HOP1_WG_DNS|HOP1_WG_PEER_PUBLIC_KEY|HOP1_WG_ENDPOINT|HOP1_WG_ALLOWED_IPS|\
			CLOUDFLARE_TUNNEL_TOKEN|CLOUDFLARE_HOSTNAME) ;;
			*) die "unknown params key: $key" ;;
		esac
		reject_chars "$key" "$value"
		printf -v "$key" '%s' "$value"
	done <"$file"
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

need() {
	local key="$1" value="$2"
	[[ -n "$value" ]] || die "$key is empty"
}

validate_params() {
	local words=() word
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
	[[ "$OWNER_ECX_ADDRESS" != "$WALLET_MNEMONIC" ]] || die "owner address and hot wallet must differ"

	is_wg_key "$VPN_WG_PRIVATE_KEY" || die "VPN_WG_PRIVATE_KEY is not a WireGuard key"
	is_wg_key "$VPN_WG_PEER_PUBLIC_KEY" || die "VPN_WG_PEER_PUBLIC_KEY is not a WireGuard key"
	is_wg_address "$VPN_WG_ADDRESS" || die "VPN_WG_ADDRESS must look like 10.0.0.2/32"
	is_ipv4 "$VPN_WG_DNS" || die "VPN_WG_DNS must be one IPv4 address"
	is_ipv4 "$VPN_PUBLIC_IPV4" || die "VPN_PUBLIC_IPV4 must be an IPv4 address"
	[[ "${VPN_WG_ALLOWED_IPS:-0.0.0.0/0, ::/0}" == "0.0.0.0/0, ::/0" ]] || die "VPN_WG_ALLOWED_IPS must be 0.0.0.0/0, ::/0"
	[[ -z "${VPN_PROVIDER:-}" || "${VPN_PROVIDER}" == "ovpn" ]] || die "VPN_PROVIDER must be ovpn"

	[[ "$CLOUDFLARE_TUNNEL_TOKEN" == eyJ* ]] || die "CLOUDFLARE_TUNNEL_TOKEN must be the eyJ string, not the install command"
	[[ "$CLOUDFLARE_HOSTNAME" != *swap.beta.ecash.com* ]] || die "CLOUDFLARE_HOSTNAME already leaked an address. Use a new name."
	[[ "$CLOUDFLARE_HOSTNAME" =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?){1,2}$ ]] || die "CLOUDFLARE_HOSTNAME is not a hostname"
	[[ "$CLOUDFLARE_HOSTNAME" != *.*.*.* ]] || die "CLOUDFLARE_HOSTNAME has too many labels for the free certificate"

	[[ "${ECX_ESPLORA_URL:-https://explorer.beta.ecash.ninja/api/}" == https://* ]] || die "ECX_ESPLORA_URL must start with https://"
	[[ "${SOLANA_RPC_URL:-https://api.mainnet-beta.solana.com}" == https://* ]] || die "SOLANA_RPC_URL must start with https://"
	[[ "${PEGIN_FEE_BPS:-100}" =~ ^[0-9]+$ ]] || die "PEGIN_FEE_BPS must be an integer"
	[[ "${PEGOUT_FEE_BPS:-100}" =~ ^[0-9]+$ ]] || die "PEGOUT_FEE_BPS must be an integer"
	[[ "${PEGIN_FLAT_FEE_SAT:-1000}" =~ ^[0-9]+$ ]] || die "PEGIN_FLAT_FEE_SAT must be an integer"
	[[ "${MIN_DEPOSIT_AMOUNT_SAT:-500}" =~ ^[0-9]+$ ]] || die "MIN_DEPOSIT_AMOUNT_SAT must be an integer"
	[[ "${ECX_DEPOSIT_MIN_CONFS:-3}" =~ ^[0-9]+$ ]] || die "ECX_DEPOSIT_MIN_CONFS must be an integer"
	(( ${ECX_DEPOSIT_MIN_CONFS:-3} >= 1 )) || die "ECX_DEPOSIT_MIN_CONFS must be at least 1"
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
	if [[ -n "${HOP1_PROVIDER:-}${HOP1_WG_PRIVATE_KEY:-}${HOP1_WG_ADDRESS:-}${HOP1_WG_PEER_PUBLIC_KEY:-}${HOP1_WG_ENDPOINT:-}" ]]; then
		HOP1_ON=1
		need HOP1_WG_PRIVATE_KEY "${HOP1_WG_PRIVATE_KEY:-}"
		need HOP1_WG_ADDRESS "${HOP1_WG_ADDRESS:-}"
		need HOP1_WG_DNS "${HOP1_WG_DNS:-}"
		need HOP1_WG_PEER_PUBLIC_KEY "${HOP1_WG_PEER_PUBLIC_KEY:-}"
		need HOP1_WG_ENDPOINT "${HOP1_WG_ENDPOINT:-}"
		is_wg_key "$HOP1_WG_PRIVATE_KEY" || die "HOP1_WG_PRIVATE_KEY is not a WireGuard key"
		is_wg_key "$HOP1_WG_PEER_PUBLIC_KEY" || die "HOP1_WG_PEER_PUBLIC_KEY is not a WireGuard key"
		is_wg_address "$HOP1_WG_ADDRESS" || die "HOP1_WG_ADDRESS must look like 10.0.0.2/32"
		is_ipv4 "$HOP1_WG_DNS" || die "HOP1_WG_DNS must be one IPv4 address"
		[[ "${HOP1_WG_ALLOWED_IPS:-0.0.0.0/0, ::/0}" == "0.0.0.0/0, ::/0" ]] || die "HOP1_WG_ALLOWED_IPS must be 0.0.0.0/0, ::/0"
		[[ "${HOP1_PROVIDER:-mullvad}" == "mullvad" ]] || die "HOP1_PROVIDER must be mullvad"
	fi
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
SOLANA_RPC_URL="${SOLANA_RPC_URL:-https://api.mainnet-beta.solana.com}"
PEGIN_FEE_BPS="${PEGIN_FEE_BPS:-100}"
PEGOUT_FEE_BPS="${PEGOUT_FEE_BPS:-100}"
PEGIN_FLAT_FEE_SAT="${PEGIN_FLAT_FEE_SAT:-1000}"
MIN_DEPOSIT_AMOUNT_SAT="${MIN_DEPOSIT_AMOUNT_SAT:-500}"
ECX_DEPOSIT_MIN_CONFS="${ECX_DEPOSIT_MIN_CONFS:-3}"
SOL_STIPEND_LAMPORTS="${SOL_STIPEND_LAMPORTS:-5000000}"
SOL_STIPEND_PRICE_SAT="${SOL_STIPEND_PRICE_SAT:-500000}"
SOL_STIPEND_USD_LABEL="${SOL_STIPEND_USD_LABEL:-0.50}"
ALLOW_LIVE_PAYOUTS=0
DATABASE_PATH=$OPT/backend/data/bridge.db
EARNINGS_CONFIG_PATH=$ETC/earnings.toml
EOF
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
	chmod 600 "$ETC/earnings.toml"
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
	[[ "$LAN_CIDR" =~ ^[0-9.]+/[0-9]+$ ]] || die "unexpected local network"
	cat > "$ETC/policy-up.sh" <<EOF
#!/bin/bash
set -euo pipefail
ip route replace default dev wg0 table 51820
ip rule del priority 50 2>/dev/null || true
ip rule add to $LAN_CIDR lookup main priority 50
EOF
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
for prio in 50 100 101 200; do
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
	[[ "$LAN_CIDR" =~ ^[0-9.]+/[0-9]+$ ]] || die "unexpected local network"
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
		oif "lo" accept
		oif "wg0" accept
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

install_node() {
	local arch sha file
	case "$(uname -m)" in
		x86_64) arch="x64"; sha="$NODE_SHA_X64" ;;
		aarch64) arch="arm64"; sha="$NODE_SHA_ARM64" ;;
		*) die "unsupported architecture: $(uname -m)" ;;
	esac
	file="node-${NODE_VERSION}-linux-${arch}.tar.xz"
	local tmp
	tmp="$(mktemp -d)"
	curl -fsSL "https://nodejs.org/dist/${NODE_VERSION}/${file}" -o "$tmp/$file"
	printf '%s  %s\n' "$sha" "$tmp/$file" | sha256sum -c -
	tar -xJf "$tmp/$file" -C /usr/local --strip-components=1
	rm -rf "$tmp"
	/usr/local/bin/node --version >/dev/null
}

require_ubuntu() {
	[[ -r /etc/os-release ]] || die "this installer is for Ubuntu Server 24.04"
	# shellcheck disable=SC1091
	. /etc/os-release
	[[ "${ID:-}" == "ubuntu" ]] || die "this installer is for Ubuntu, found ${ID:-unknown}"
	[[ "${VERSION_ID:-0}" == 24.* || "${VERSION_ID:-0}" > 24 ]] || die "Ubuntu 24.04 or newer is required, found ${VERSION_ID:-unknown}"
}

copy_tree() {
	mkdir -p "$OPT/src" "$OPT/backend/data" "$OPT/frontend"
	local rsync_bin="rsync"
	command -v rsync >/dev/null || rsync_bin=""
	if [[ -n "$rsync_bin" ]]; then
		rsync -a --delete \
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
			-cf - . | tar -C "$OPT/src" -xf -
	fi
}

run_as_backend() {
	local subcommand="$1"
	systemd-run --quiet --pipe --wait --collect \
		--uid=bridgebackend --gid=bridgebackend \
		--working-directory="$OPT/backend" \
		--property=EnvironmentFile="$ETC/backend.env" \
		"$OPT/backend/swap-backend" "$subcommand"
}

cmd_check() {
	local file="$1"
	parse_params "$file"
	validate_params
	printf 'params.conf is complete. Secrets were not printed.\n'
	if ((HOP1_ON)); then
		printf 'Hop 1 is set. The installer will route OVPN through Mullvad.\n'
	else
		printf 'Hop 1 is empty. OVPN will be the only VPN.\n'
	fi
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
	systemctl daemon-reload
	systemctl enable --now ecash-bridge-cloudflared.service
	systemctl restart ecash-bridge-backend.service
	printf 'Payouts are on and the Cloudflare tunnel is started.\n'
	printf 'Customers use https://%s once the connector shows connected.\n' "$(awk -F= '/^CLOUDFLARE_HOSTNAME=/{print $2}' "$ETC/public.env")"
}

cmd_install() {
	local file="$1"
	[[ "$(id -u)" -eq 0 ]] || die "install must be run as root"
	require_ubuntu
	parse_params "$file"
	validate_params

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

	PHYS="$(ip -4 route show default | awk '{print $5; exit}')"
	[[ -n "$PHYS" ]] || die "no IPv4 default route. Connect the server to its local network first."
	ORIG_GW="$(ip -4 route show default | awk '{print $3; exit}')"
	local cidr
	cidr="$(ip -4 addr show dev "$PHYS" | awk '/inet / {print $2; exit}')"
	[[ -n "$cidr" ]] || die "physical interface $PHYS has no IPv4 address"
	LAN_CIDR="$(ip -4 route show dev "$PHYS" proto kernel | awk '{print $1; exit}')"
	[[ "$LAN_CIDR" =~ ^[0-9.]+/[0-9]+$ ]] || die "could not find the IPv4 subnet on $PHYS"

	export DEBIAN_FRONTEND=noninteractive
	apt-get update
	apt-get install -y build-essential pkg-config libssl-dev curl ca-certificates wireguard openresolv nftables rsync python3
	install_node
	if ! command -v cargo >/dev/null; then
		curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
	fi
	# shellcheck disable=SC1091
	[[ -f /root/.cargo/env ]] && . /root/.cargo/env

	install -d -m 700 "$ETC" /var/lib/ecash-bridge /etc/wireguard
	install -m 600 "$file" "$ETC/params.conf"
	printf 'CLOUDFLARE_HOSTNAME=%s\n' "$CLOUDFLARE_HOSTNAME" > "$ETC/public.env"
	chmod 644 "$ETC/public.env"
	collect_ssh_peers

	sysctl -w net.ipv6.conf.all.disable_ipv6=1
	sysctl -w net.ipv6.conf.default.disable_ipv6=1
	printf 'net.ipv6.conf.all.disable_ipv6=1\nnet.ipv6.conf.default.disable_ipv6=1\n' > /etc/sysctl.d/99-ecash-bridge.conf

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
		printf '[Unit]\nAfter=nftables.service\nWants=nftables.service\n' > /etc/systemd/system/wg-quick@hop1.service.d/firewall.conf
	else
		write_ssh_routes
		write_wg wg0 "$VPN_WG_PRIVATE_KEY" "$VPN_WG_ADDRESS" "$VPN_WG_PEER_PUBLIC_KEY" "$VPN_IP" "$VPN_PORT" "$(cat <<EOF
DNS = $VPN_WG_DNS
PostUp = $ETC/ssh-up.sh
PreDown = $ETC/ssh-down.sh
EOF
)"
		rm -f /etc/systemd/system/wg-quick@wg0.service.d/hop1.conf /etc/wireguard/hop1.conf
		systemctl disable --now wg-quick@hop1.service >/dev/null 2>&1 || true
		install -d /etc/systemd/system/wg-quick@wg0.service.d
		printf '[Unit]\nAfter=nftables.service\nWants=nftables.service\n' > /etc/systemd/system/wg-quick@wg0.service.d/firewall.conf
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
	((handshake)) || die "wg0 has no handshake. The firewall is on. SSH from the local network, or the session that started this install, still works. Fix the VPN fields and run the installer again."
	unset attempt
	local egress
	egress="$(curl --max-time 20 -fsS https://ipv4.icanhazip.com | tr -d '[:space:]' || true)"
	[[ "$egress" == "$VPN_PUBLIC_IPV4" ]] || die "public address is not VPN_PUBLIC_IPV4. The firewall is on. SSH from the local network, or the session that started this install, still works. Fix the VPN fields and run the installer again."

	if ! command -v cloudflared >/dev/null; then
		install -d -m 755 /usr/share/keyrings
		local gpg_tmp
		gpg_tmp="$(mktemp)"
		curl -fsSL https://pkg.cloudflare.com/cloudflare-main.gpg | gpg --dearmor -o "$gpg_tmp"
		install -m 644 "$gpg_tmp" /usr/share/keyrings/cloudflare-main.gpg
		rm -f "$gpg_tmp"
		printf 'deb [signed-by=/usr/share/keyrings/cloudflare-main.gpg] https://pkg.cloudflare.com/cloudflared any main\n' > /etc/apt/sources.list.d/cloudflared.list
		apt-get update
		apt-get install -y cloudflared
	fi
	systemctl disable --now cloudflared.service >/dev/null 2>&1 || true

	copy_tree
	( cd "$OPT/src/backend" && cargo build --release )
	install -m 755 "$OPT/src/backend/target/release/swap-backend" "$OPT/backend/swap-backend"
	/usr/local/bin/npm install -g pnpm@9
	( cd "$OPT/src/frontend" && pnpm install --frozen-lockfile && pnpm build )
	rm -rf "$OPT/frontend"
	mkdir -p "$OPT/frontend"
	cp -a "$OPT/src/frontend/build" "$OPT/src/frontend/package.json" "$OPT/src/frontend/node_modules" "$OPT/frontend/"

	id bridgebackend >/dev/null 2>&1 || useradd --system --home-dir "$OPT/backend" --shell /usr/sbin/nologin bridgebackend
	id bridgeweb >/dev/null 2>&1 || useradd --system --home-dir "$OPT/frontend" --shell /usr/sbin/nologin bridgeweb
	id cloudflared >/dev/null 2>&1 || useradd --system --home-dir /var/lib/ecash-bridge --shell /usr/sbin/nologin cloudflared
	install -d -m 700 -o bridgebackend -g bridgebackend "$OPT/backend/data"
	chown -R bridgeweb:bridgeweb "$OPT/frontend"
	chown root:root "$OPT/backend/swap-backend"

	write_env_file "$ETC/backend.env"
	write_earnings
	printf 'PUBLIC_WECX_MINT=%s\nPUBLIC_ASSET_TICKER=%s\n' "$WECX_MINT_ADDRESS" "${ASSET_TICKER:-bECX}" > "$ETC/frontend.env"
	chmod 644 "$ETC/frontend.env"
	printf 'TUNNEL_TOKEN=%s\n' "$CLOUDFLARE_TUNNEL_TOKEN" > "$ETC/cloudflared.env"
	chown root:cloudflared "$ETC/cloudflared.env"
	chmod 640 "$ETC/cloudflared.env"

	install -m 644 "$ROOT/deploy/ecash-bridge-backend.service" /etc/systemd/system/ecash-bridge-backend.service
	install -m 644 "$ROOT/deploy/ecash-bridge-frontend.service" /etc/systemd/system/ecash-bridge-frontend.service
	install -m 644 "$ROOT/deploy/edge/cloudflared.service" /etc/systemd/system/ecash-bridge-cloudflared.service
	install -d /etc/systemd/system/ecash-bridge-backend.service.d /etc/systemd/system/ecash-bridge-frontend.service.d /etc/systemd/system/ecash-bridge-cloudflared.service.d
	printf '[Unit]\nAfter=nftables.service wg-quick@wg0.service\nRequires=wg-quick@wg0.service\nPartOf=wg-quick@wg0.service\n' > /etc/systemd/system/ecash-bridge-backend.service.d/vpn.conf
	printf '[Unit]\nAfter=nftables.service wg-quick@wg0.service\nRequires=wg-quick@wg0.service\nPartOf=wg-quick@wg0.service\n[Service]\nEnvironmentFile=%s\n' "$ETC/frontend.env" > /etc/systemd/system/ecash-bridge-frontend.service.d/vpn.conf
	printf '[Unit]\nAfter=nftables.service wg-quick@wg0.service\nRequires=wg-quick@wg0.service\nPartOf=wg-quick@wg0.service\n' > /etc/systemd/system/ecash-bridge-cloudflared.service.d/vpn.conf
	systemctl daemon-reload

	local ecx_addr sol_addr
	ecx_addr="$(run_as_backend address | awk '/\[ecx\]/ {print $2}')"
	sol_addr="$(run_as_backend solana-address | awk '{print $NF}')"
	[[ "$ecx_addr" == bc1q* ]] || die "could not derive the ECX deposit address"
	[[ -n "$sol_addr" ]] || die "could not derive the Solana treasury address"
	cat > /var/lib/ecash-bridge/deposit-addresses.txt <<EOF
ECX deposit address: $ecx_addr
Solana treasury: $sol_addr
EOF
	chmod 644 /var/lib/ecash-bridge/deposit-addresses.txt

	systemctl enable --now ecash-bridge-backend.service ecash-bridge-frontend.service
	systemctl disable --now ecash-bridge-cloudflared.service >/dev/null 2>&1 || true

	printf '\nInstalled. Payouts are off and the public tunnel is not started.\n'
	cat /var/lib/ecash-bridge/deposit-addresses.txt
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
		enable) cmd_enable "${2:-}" ;;
		"") usage; die "a params file is required" ;;
		*) cmd_install "$1" ;;
	esac
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
	main "$@"
fi
