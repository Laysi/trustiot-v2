#!/bin/bash
# Exploration for Laysi/trustiot-v2#867, second run, 2026-11-06, in a throwaway container. swtpm through its own TCTI has
# no resource manager: every tool's transient objects and sessions are flushed after it (t()).
set -uo pipefail
export LC_ALL=C DEBIAN_FRONTEND=noninteractive
snap=20261030T000000Z
src() {
	cat <<EOS
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
EOS
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

W=/w
mkdir -p $W
cd $W || exit 1
TEMPLATE=(--hierarchy=o --hash-algorithm=sha256 --key-algorithm=ecc256:aes128cfb --attributes='fixedtpm|fixedparent|sensitivedataorigin|userwithauth|noda|restricted|decrypt')

declare -A PIDS
start() {
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
flush() {
	TPM2TOOLS_TCTI=swtpm:path=$W/$1.sock tpm2_flushcontext --transient-object >/dev/null 2>&1
	TPM2TOOLS_TCTI=swtpm:path=$W/$1.sock tpm2_flushcontext --loaded-session >/dev/null 2>&1
	TPM2TOOLS_TCTI=swtpm:path=$W/$1.sock tpm2_flushcontext --saved-session >/dev/null 2>&1
}
t() {
	local n=$1 code
	shift
	TPM2TOOLS_TCTI=swtpm:path=$W/$n.sock "$@"
	code=$?
	flush "$n"
	return $code
}
fp() {
	local n=$1
	rm -f $W/fp.ctx $W/fp.name
	t "$n" tpm2_createprimary "${TEMPLATE[@]}" --key-context=$W/fp.ctx >/dev/null 2>$W/fp.err || {
		echo "fingerprint: createprimary failed: $(grep -o 'Esys_CreatePrimary(0x[0-9A-F]*) - .*' $W/fp.err)"
		return 1
	}
	t "$n" tpm2_readpublic --object-context=$W/fp.ctx --name=$W/fp.name >/dev/null 2>$W/fp.err || {
		echo "fingerprint: readpublic failed: $(tail -1 $W/fp.err)"
		return 1
	}
	echo "fingerprint: $(od -An -tx1 -v $W/fp.name | tr -d ' \n')"
}
srk() {
	rm -f $W/srk.name
	if t "$1" tpm2_readpublic --object-context=0x81000001 --name=$W/srk.name >/dev/null 2>$W/srk.err; then
		echo "srk 0x81000001: $(od -An -tx1 -v $W/srk.name | tr -d ' \n')"
	else
		echo "srk 0x81000001: none ($(grep -o 'Esys_ReadPublic(0x[0-9A-F]*) - .*' $W/srk.err | head -1))"
	fi
}
unseal() {
	local start end out code
	start=$(date +%s%N)
	out=$(SYSTEMD_TPM2_DEVICE=swtpm:path=$W/$1.sock cryptsetup open --test-passphrase --token-only --token-type systemd-tpm2 "$2" 2>&1)
	code=$?
	end=$(date +%s%N)
	flush "$1"
	echo "unseal: exit $code in $(((end - start) / 1000000)) ms"
	[[ -z $out ]] || printf '%s\n' "$out" | sed 's/^/  | /'
}
look() {
	unseal "$1" $W/luks.img
	fp "$1"
	srk "$1"
	t "$1" tpm2_getcap properties-variable 2>&1 | grep -E 'ownerAuthSet|inLockout|LOCKOUT_COUNTER'
}

echo "== enroll TPM A into a LUKS2 file that already holds the setup's tokens 1 and 2"
head -c 48 /dev/urandom | base64 -w0 >$W/key
chmod 600 $W/key
truncate -s 64M $W/luks.img
cryptsetup luksFormat --batch-mode --type luks2 --pbkdf pbkdf2 --pbkdf-force-iterations 1000 --uuid 9da7f680-5912-4d5c-bca6-d46c83e064f8 --label trustiot-data --key-file $W/key $W/luks.img
start A $W/tpmA
fp A
printf '{"type":"trustiot-tpm-owner","keyslots":[],"owner":"%s"}\n' "$(od -An -tx1 -v $W/fp.name | tr -d ' \n')" >$W/owner.json
cryptsetup token import --token-id 1 --json-file $W/owner.json $W/luks.img
printf '{"type":"trustiot-back-to-factory","keyslots":[],"reason":"tpm-owner-changed","sealed_owner":"000b%s","found_owner":"000b%s"}\n' "$(printf 'a%.0s' {1..64})" "$(printf 'b%.0s' {1..64})" >$W/mark.json
cryptsetup token import --token-id 2 --json-file $W/mark.json $W/luks.img
systemd-cryptenroll --unlock-key-file=$W/key --tpm2-device=swtpm:path=$W/A.sock --tpm2-pcrs= --tpm2-public-key= --tpm2-pcrlock= --tpm2-with-pin=no $W/luks.img
echo "enroll exit $?"
flush A
cryptsetup luksKillSlot --batch-mode $W/luks.img 0 </dev/null
echo "-- systemd-cryptenroll listing"
systemd-cryptenroll $W/luks.img
echo "-- tokens (luksDump)"
cryptsetup luksDump $W/luks.img | sed -n '/^Tokens:/,/^Digests:/p' | grep -E '^  [0-9]+: |Keyslot:'
echo "-- token exports"
for id in 0 1 2; do
	cryptsetup token export --token-id $id $W/luks.img | sed 's/"tpm2-blob":"[^"]*"/"tpm2-blob":"..."/; s/"tpm2_srk":"[^"]*"/"tpm2_srk":"..."/'
	echo
done
echo "-- persistent handles"
t A tpm2_getcap handles-persistent

echo "== A (the TPM it was sealed to)"
look A
echo "-- A again"
fp A
stop A

echo "== B (another TPM, fresh)"
start B $W/tpmB
look B
stop B

echo "== B with its own standard SRK at 0x81000001"
cp -a $W/tpmB $W/tpmB2
start B2 $W/tpmB2
t B2 tpm2_createprimary "${TEMPLATE[@]}" --key-context=$W/b2.ctx >/dev/null
t B2 tpm2_evictcontrol --hierarchy=o --object-context=$W/b2.ctx 0x81000001 >/dev/null
look B2
stop B2

echo "== A cleared (TPM2_Clear, lockout hierarchy)"
cp -a $W/tpmA $W/tpmAc
start Ac $W/tpmAc
t Ac tpm2_clear --auth-hierarchy=l
echo "clear exit $?"
look Ac
stop Ac

echo "== A with the SRK evicted (owner seed unchanged)"
cp -a $W/tpmA $W/tpmAe
start Ae $W/tpmAe
t Ae tpm2_evictcontrol --hierarchy=o --object-context=0x81000001 >/dev/null
echo "evict exit $?"
look Ae
stop Ae

echo "== A in dictionary-attack lockout"
cp -a $W/tpmA $W/tpmAl
start Al $W/tpmAl
t Al tpm2_dictionarylockout --setup-parameters --max-tries=1 --recovery-time=3600 --lockout-recovery-time=3600
t Al tpm2_createprimary --hierarchy=o --key-algorithm=ecc --key-auth=secret --attributes='fixedtpm|fixedparent|sensitivedataorigin|userwithauth|sign' --key-context=$W/da.ctx >/dev/null
t Al tpm2_evictcontrol --hierarchy=o --object-context=$W/da.ctx 0x81000005 >/dev/null
echo data >$W/msg
t Al tpm2_sign --key-context=0x81000005 --auth=wrong --hash-algorithm=sha256 --signature=$W/sig $W/msg 2>&1 | grep -o 'Esys_Sign(0x[0-9A-F]*) - .*'
t Al tpm2_sign --key-context=0x81000005 --auth=secret --hash-algorithm=sha256 --signature=$W/sig $W/msg 2>&1 | grep -o 'Esys_Sign(0x[0-9A-F]*) - .*'
look Al
stop Al

echo "== B in lockout"
cp -a $W/tpmB $W/tpmBl
start Bl $W/tpmBl
t Bl tpm2_dictionarylockout --setup-parameters --max-tries=1 --recovery-time=3600 --lockout-recovery-time=3600
t Bl tpm2_createprimary --hierarchy=o --key-algorithm=ecc --key-auth=secret --attributes='fixedtpm|fixedparent|sensitivedataorigin|userwithauth|sign' --key-context=$W/da2.ctx >/dev/null
t Bl tpm2_evictcontrol --hierarchy=o --object-context=$W/da2.ctx 0x81000005 >/dev/null
t Bl tpm2_sign --key-context=0x81000005 --auth=wrong --hash-algorithm=sha256 --signature=$W/sig $W/msg >/dev/null 2>&1
look Bl
stop Bl

echo "== B with an owner password"
cp -a $W/tpmB $W/tpmBo
start Bo $W/tpmBo
t Bo tpm2_changeauth --object-context=o ownerpass
look Bo
stop Bo

echo "== no TPM"
unseal none $W/luks.img
TPM2TOOLS_TCTI=device:/dev/tpmrm0 tpm2_getcap properties-variable >/dev/null 2>&1
echo "getcap without a TPM: exit $?"

echo "== the token import into a header file keeps the ids, and enrolling takes token 0"
truncate -s 2G $W/standin
cryptsetup luksFormat --batch-mode --type luks2 --pbkdf pbkdf2 --pbkdf-force-iterations 1000 --uuid 9da7f680-5912-4d5c-bca6-d46c83e064f8 --label trustiot-data-new --key-file $W/key \
	--header $W/hdr --offset 32768 --luks2-metadata-size 16384 --luks2-keyslots-size 16744448 $W/standin
cryptsetup token import --token-id 1 --json-file $W/owner.json $W/hdr && cryptsetup token import --token-id 2 --json-file $W/mark.json $W/hdr
echo "import exit $?; size $(stat -c %s $W/hdr)"
truncate -s 2G $W/part
dd if=$W/hdr of=$W/part conv=notrunc status=none
start A $W/tpmA
systemd-cryptenroll --unlock-key-file=$W/key --tpm2-device=swtpm:path=$W/A.sock --tpm2-pcrs= --tpm2-public-key= --tpm2-pcrlock= --tpm2-with-pin=no $W/part
flush A
cryptsetup luksDump $W/part | sed -n '/^Tokens:/,/^Digests:/p' | grep -E '^  [0-9]+: |Keyslot:'
unseal A $W/part
stop A
echo "== done"
