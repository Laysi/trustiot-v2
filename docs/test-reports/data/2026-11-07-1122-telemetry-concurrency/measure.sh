set -u
img=docker.io/clickhouse/clickhouse-server:26.8@sha256:110d3d1f8e1ed7e451a6b5d85a16761e948ad43bc2d9ddf10b155964a8b2b40c
eng=podman; command -v podman >/dev/null || eng=docker
name=m1122-$$
trap '$eng rm --force --volumes $name >/dev/null 2>&1' EXIT
$eng run -d --name $name -e CLICKHOUSE_USER=adm -e CLICKHOUSE_PASSWORD=adm -e CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1 $img >/dev/null
adm() { $eng exec $name clickhouse-client --user adm --password adm -q "$1"; }
for i in $(seq 1 120); do adm "SELECT 1" >/dev/null 2>&1 && break; sleep 1; done
adm "SELECT version()"
adm "CREATE SETTINGS PROFILE p SETTINGS readonly = 1 CONST, max_concurrent_queries_for_user = 1 CONST"
adm "CREATE USER t IDENTIFIED WITH plaintext_password BY 'tp' SETTINGS PROFILE 'p'"
adm "CREATE QUOTA q KEYED BY user_name FOR INTERVAL 1 hour MAX queries = 1000, errors = 1000 TO t"
usage() { adm "SELECT 'usage', queries, errors FROM system.quotas_usage WHERE quota_name = 'q' AND quota_key = 't' FORMAT TSV"; }
http() { $eng exec $name bash -c "exec 3<>/dev/tcp/127.0.0.1/8123; printf 'POST /?wait_end_of_query=1 HTTP/1.0\r\nAuthorization: Basic dDp0cA==\r\nContent-Length: ${#1}\r\n\r\n%s' '$1' >&3; head -c 300 <&3 | tr '\r\n' '  '"; echo; }
echo "--- start"; usage
echo "--- one successful SELECT over HTTP"; http "SELECT 1"; usage
echo "--- hold the one slot with sleep(3) over HTTP, then a second SELECT over HTTP"
( http "SELECT sleep(3)" > /dev/null ) &
for i in $(seq 1 50); do n=$(adm "SELECT count() FROM system.processes WHERE user = 't'"); [ "$n" = 1 ] && break; sleep 0.1; done
echo "holder running: $n"
http "SELECT 1"
wait
usage
echo "--- the refusal in query_log"
adm "SYSTEM FLUSH LOGS"
adm "SELECT type, exception_code, query FROM system.query_log WHERE user = 't' ORDER BY event_time_microseconds FORMAT TSV"
