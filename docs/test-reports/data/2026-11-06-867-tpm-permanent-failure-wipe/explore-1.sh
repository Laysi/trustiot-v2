#!/bin/bash
# Exploration for Laysi/trustiot-v2#867, run on 2026-11-06 in a throwaway container: what systemd-cryptenroll 257, the
# systemd-tpm2 token plugin, cryptsetup 2.7.5 and tpm2-tools 5.7 report for another TPM, a cleared
# TPM, an evicted SRK and a TPM in dictionary-attack lockout, with swtpm.
set -uo pipefail
export LC_ALL=C DEBIAN_FRONTEND=noninteractive
snap=20261030T000000Z
src() {
	cat <<EOF
Types: deb
URIs: $1://snapshot.debian.org/archive/debian/$snap
Suites: trixie trixie-updates
Components: main
Signed-By: /usr/share/keyrings/debian-archive-keyring.pgp
Check-Valid-Until: no

Types: deb
URIs: $1://snapshot.debian.org/archive/debian-security/$snap
Suites: trixie-security
Components: main
Signed-By: /usr/share/keyrings/debian-archive-keyring.pgp
Check-Valid-Until: no
EOF
}
rm -f /etc/apt/sources.list
src http >/etc/apt/sources.list.d/debian.sources
apt-get -qq -o Acquire::Retries=5 update >/dev/null
apt-get -qq -o Acquire::Retries=5 install -y --no-install-recommends ca-certificates=20250419+deb13u1 >/dev/null
src https >/etc/apt/sources.list.d/debian.sources
apt-get -qq -o Acquire::Retries=5 update >/dev/null
apt-get -qq -o Acquire::Retries=5 install -y --no-install-recommends swtpm=0.7.1-1.5 swtpm-tools libtss2-tcti-swtpm0t64=4.1.3-1.2 \
	tpm2-tools=5.7-1+b1 cryptsetup-bin=2:2.7.5-2 systemd-cryptsetup=257.13-1~deb13u1 >/dev/null 2>&1 || {
	echo "apt failed"
	exit 1
}
dpkg-query -W swtpm swtpm-tools tpm2-tools cryptsetup-bin systemd-cryptsetup systemd libtss2-tcti-swtpm0t64

W=/w
mkdir -p $W
cd $W || exit 1
TEMPLATE=(-g sha256 -G ecc256:aes128cfb -a 'fixedtpm|fixedparent|sensitivedataorigin|userwithauth|noda|restricted|decrypt')

declare -A PIDS
start() { # name statedir
	local n=$1 d=$2
	mkdir -p "$d"
	rm -f $W/$n.sock $W/$n.sock.ctrl
	swtpm socket --tpm2 --tpmstate dir="$d" --server type=unixio,path=$W/$n.sock --ctrl type=unixio,path=$W/$n.sock.ctrl --flags startup-clear --log file=$W/swtpm-$n.log,level=1 &
	PIDS[$n]=$!
	for _ in $(seq 100); do [[ -S $W/$n.sock ]] && break; sleep 0.1; done
}
stop() {
	kill "${PIDS[$1]}" 2>/dev/null
	wait "${PIDS[$1]}" 2>/dev/null
}
t() { TPM2TOOLS_TCTI=swtpm:path=$W/$1.sock "${@:2}"; }
fp() { # name -> name of the primary from TEMPLATE
	local n=$1
	t "$n" tpm2_createprimary -C o "${TEMPLATE[@]}" -c $W/fp-$n.ctx >$W/fp-$n.yaml 2>$W/fp-$n.err || {
		echo "createprimary failed: $(cat $W/fp-$n.err)"
		return 1
	}
	t "$n" tpm2_readpublic -c $W/fp-$n.ctx -n $W/fp-$n.name >/dev/null 2>&1 || {
		echo "readpublic failed"
		return 1
	}
	od -An -tx1 -v $W/fp-$n.name | tr -d ' \n'
	echo
}
unseal() { # name file
	SYSTEMD_TPM2_DEVICE=swtpm:path=$W/$1.sock cryptsetup open --test-passphrase --token-only --token-type systemd-tpm2 "$2" 2>&1
	echo "exit $?"
}

echo "== createprimary output format (fresh TPM A)"
start A $W/tpmA
t A tpm2_createprimary -C o "${TEMPLATE[@]}" -c $W/x.ctx
echo "exit $?"
echo "-- readpublic"
t A tpm2_readpublic -c $W/x.ctx -n $W/x.name
echo "exit $?"
echo "-- getcap properties-variable"
t A tpm2_getcap properties-variable
echo "-- getcap handles-persistent (fresh)"
t A tpm2_getcap handles-persistent

echo "== LUKS2 file, enroll TPM A"
head -c 48 /dev/urandom | base64 -w0 >$W/key
truncate -s 64M $W/luks.img
cryptsetup luksFormat --batch-mode --type luks2 --pbkdf pbkdf2 --pbkdf-force-iterations 1000 --uuid 9da7f680-5912-4d5c-bca6-d46c83e064f8 --label trustiot-data --key-file $W/key $W/luks.img
time systemd-cryptenroll --unlock-key-file=$W/key --tpm2-device=swtpm:path=$W/A.sock --tpm2-pcrs= --tpm2-public-key= --tpm2-pcrlock= --tpm2-with-pin=no $W/luks.img
echo "exit $?"
cryptsetup luksKillSlot --batch-mode $W/luks.img 0 </dev/null
systemd-cryptenroll $W/luks.img
echo "-- persistent handles after enroll"
t A tpm2_getcap handles-persistent
echo "-- name of 0x81000001"
t A tpm2_readpublic -c 0x81000001 -n $W/srk.name >/dev/null && od -An -tx1 -v $W/srk.name | tr -d ' \n'
echo
echo "-- fingerprint of A (twice)"
fp A
fp A
echo "-- tpm2_srk field, decoded (first 48 bytes)"
cryptsetup token export --token-id 0 $W/luks.img | sed -n 's/.*"tpm2_srk":"\([^"]*\)".*/\1/p' | base64 -d | od -An -tx1 -v | head -4
echo "-- token 0 keys"
cryptsetup token export --token-id 0 $W/luks.img | sed 's/"tpm2-blob":"[^"]*"/"tpm2-blob":"..."/; s/"tpm2_srk":"[^"]*"/"tpm2_srk":"..."/'
echo
echo "-- unseal with A"
time unseal A $W/luks.img
stop A

echo "== another TPM B (fresh)"
start B $W/tpmB
unseal B $W/luks.img
echo "-- fingerprint of B"
fp B
t B tpm2_getcap handles-persistent
stop B

echo "== TPM B with its own standard SRK at 0x81000001"
cp -a $W/tpmB $W/tpmB2
start B2 $W/tpmB2
t B2 tpm2_createprimary -C o "${TEMPLATE[@]}" -c $W/b2.ctx >/dev/null
t B2 tpm2_evictcontrol -C o -c $W/b2.ctx 0x81000001
unseal B2 $W/luks.img
stop B2

echo "== TPM A cleared"
cp -a $W/tpmA $W/tpmAc
start Ac $W/tpmAc
t Ac tpm2_clear -c l
echo "clear exit $?"
unseal Ac $W/luks.img
fp Ac
t Ac tpm2_getcap handles-persistent
stop Ac

echo "== TPM A, SRK evicted (owner seed unchanged)"
cp -a $W/tpmA $W/tpmAe
start Ae $W/tpmAe
t Ae tpm2_evictcontrol -C o -c 0x81000001
echo "evict exit $?"
unseal Ae $W/luks.img
fp Ae
stop Ae

echo "== TPM A in dictionary-attack lockout"
cp -a $W/tpmA $W/tpmAl
start Al $W/tpmAl
t Al tpm2_dictionarylockout --setup-parameters --max-tries=1 --recovery-time=3600 --lockout-recovery-time=3600
echo "dictionarylockout exit $?"
t Al tpm2_createprimary -C o -G ecc -p secret -a 'fixedtpm|fixedparent|sensitivedataorigin|userwithauth|sign' -c $W/da.ctx >/dev/null
echo data >$W/msg
t Al tpm2_sign -c $W/da.ctx -p wrong -g sha256 -o $W/sig $W/msg
echo "sign with a wrong password exit $?"
t Al tpm2_sign -c $W/da.ctx -p secret -g sha256 -o $W/sig $W/msg
echo "sign with the right password exit $?"
t Al tpm2_getcap properties-variable | grep -E "inLockout|LOCKOUT|AUTH_FAIL"
echo "-- unseal A in lockout"
unseal Al $W/luks.img
echo "-- fingerprint A in lockout"
fp Al
stop Al

echo "== TPM B in lockout (another TPM)"
cp -a $W/tpmB $W/tpmBl
start Bl $W/tpmBl
t Bl tpm2_dictionarylockout --setup-parameters --max-tries=1 --recovery-time=3600 --lockout-recovery-time=3600 >/dev/null
t Bl tpm2_createprimary -C o -G ecc -p secret -a 'fixedtpm|fixedparent|sensitivedataorigin|userwithauth|sign' -c $W/da2.ctx >/dev/null
t Bl tpm2_sign -c $W/da2.ctx -p wrong -g sha256 -o $W/sig $W/msg >/dev/null 2>&1
t Bl tpm2_getcap properties-variable | grep -E "inLockout"
fp Bl
stop Bl

echo "== TPM B with an owner password"
cp -a $W/tpmB $W/tpmBo
start Bo $W/tpmBo
t Bo tpm2_changeauth -c o ownerpass
fp Bo
stop Bo

echo "== no TPM at all"
SYSTEMD_TPM2_DEVICE=device:/dev/tpmrm0 cryptsetup open --test-passphrase --token-only --token-type systemd-tpm2 $W/luks.img 2>&1
echo "exit $?"
TPM2TOOLS_TCTI=device:/dev/tpmrm0 tpm2_getcap properties-variable 2>&1 | tail -3
echo "exit $?"

echo "== custom tokens"
printf '{"type":"trustiot-tpm","keyslots":[],"srk_name":"000b%s"}' "$(printf '%064d' 0)" >$W/t1.json
cryptsetup token import --token-id 1 --json-file $W/t1.json $W/luks.img
echo "import 1 exit $?"
cryptsetup token import --token-id 1 --json-file $W/t1.json $W/luks.img
echo "import 1 again exit $?"
cryptsetup token import --token-id 1 --token-replace --json-file $W/t1.json $W/luks.img
echo "import 1 replace exit $?"
cryptsetup token export --token-id 1 $W/luks.img
echo
echo "export exit $?"
cryptsetup token export --token-id 5 $W/luks.img
echo "export missing exit $?"
systemd-cryptenroll $W/luks.img
start A $W/tpmA
unseal A $W/luks.img
SYSTEMD_TPM2_DEVICE=swtpm:path=$W/A.sock cryptsetup open --test-passphrase --token-only $W/luks.img 2>&1
echo "token-only without type exit $?"
stop A
cryptsetup config --label trustiot-data-new $W/luks.img
cryptsetup luksDump $W/luks.img | sed -n '/^Tokens:/,/^Digests:/p' | grep -v "tpm2-blob\|tpm2_srk\|^\s\s\s\s\s\s\s\s\s\s\s\s"
cryptsetup token remove --token-id 1 $W/luks.img
echo "remove exit $?"

echo "== token import into a detached header file, then enroll"
truncate -s 2G $W/standin
cryptsetup luksFormat --batch-mode --type luks2 --pbkdf pbkdf2 --pbkdf-force-iterations 1000 --uuid 9da7f680-5912-4d5c-bca6-d46c83e064f8 --label trustiot-data-new --key-file $W/key \
	--header $W/hdr --offset 32768 --luks2-metadata-size 16384 --luks2-keyslots-size 16744448 $W/standin
printf '{"type":"trustiot-wipe","keyslots":[],"phase":"wiped"}' >$W/t2.json
cryptsetup token import --token-id 2 --json-file $W/t2.json $W/hdr
echo "import into header file exit $?"
stat -c %s $W/hdr
blkid -p -o export $W/hdr
truncate -s 2G $W/part
dd if=$W/hdr of=$W/part conv=notrunc status=none
start A $W/tpmA
systemd-cryptenroll --unlock-key-file=$W/key --tpm2-device=swtpm:path=$W/A.sock --tpm2-pcrs= --tpm2-public-key= --tpm2-pcrlock= --tpm2-with-pin=no $W/part
cryptsetup luksDump --dump-json-metadata $W/part | tr ',' '\n' | grep -E '"type"|"keyslots"|^"[0-9]"|tokens'
stop A
echo "== done"
