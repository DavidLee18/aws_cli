set -u
export AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE
export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY
export AWS_DEFAULT_REGION=us-west-2
unset AWS_PROFILE AWS_SESSION_TOKEN 2>/dev/null || true
AWSC=./target/debug/awsc
pass=0; fail=0
cmp_one() {
  desc="$1"; shift
  ref=$(aws "$@" 2>&1); rc_ref=$?
  # Pin our clock to the timestamp the reference just used.
  stamp=$(printf '%s' "$ref" | grep -o 'X-Amz-Date=[0-9TZ]*' | head -1 | cut -d= -f2)
  if [ -z "$stamp" ]; then stamp=$(printf '%s' "$ref" | grep -o 'password=[0-9]\{8\}T[0-9]\{6\}Z' | head -1 | sed 's/password=//'); fi
  if [ -n "$stamp" ]; then
    epoch=$(python3 -c "import datetime,sys;print(int(datetime.datetime.strptime(sys.argv[1].rstrip('Z'),'%Y%m%dT%H%M%S').replace(tzinfo=datetime.timezone.utc).timestamp()))" "$stamp")
    export AWSC_FIXED_TIME=$epoch
  fi
  ours=$($AWSC "$@" 2>&1); rc_ours=$?
  unset AWSC_FIXED_TIME
  if [ "$ref" = "$ours" ] && [ "$rc_ref" = "$rc_ours" ]; then
    echo "PASS  $desc"; pass=$((pass+1))
  else
    echo "FAIL  $desc (rc ref=$rc_ref ours=$rc_ours)"
    diff <(printf '%s\n' "$ref") <(printf '%s\n' "$ours") | head -12
    fail=$((fail+1))
  fi
}
cmp_one "rds token basic" rds generate-db-auth-token --hostname MyDB.123456789012.us-west-2.rds.amazonaws.com --port 3306 --username jane_doe
cmp_one "rds token port 443" rds generate-db-auth-token --hostname mydb.us-west-2.rds.amazonaws.com --port 443 --username jane_doe
cmp_one "rds token space in user" rds generate-db-auth-token --hostname mydb.us-west-2.rds.amazonaws.com --port 3306 --username "jane doe"
cmp_one "rds missing arg" rds generate-db-auth-token --hostname h --port 1
cmp_one "rds bad port" rds generate-db-auth-token --hostname h --port abc --username u
cmp_one "rds unknown flag" rds generate-db-auth-token --hostname h --port 1 --username u --nope z
echo "pass=$pass fail=$fail"
