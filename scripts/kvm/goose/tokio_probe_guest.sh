export HOME=/root
echo A-TIMEOUT5-MULTI
NETTEST_TIMEOUT=5 /bin/nettest-reqwest get http://10.0.2.2:18080/ 2>&1 | grep -a "RESULT"
echo B-TIMEOUT9-MULTI
NETTEST_TIMEOUT=9 /bin/nettest-reqwest get http://10.0.2.2:18080/ 2>&1 | grep -a "RESULT"
echo C-TIMEOUT5-CURRENT
NETTEST_RT=current NETTEST_TIMEOUT=5 /bin/nettest-reqwest get http://10.0.2.2:18080/ 2>&1 | grep -a "RESULT"
echo D-NOTIMEOUT-CURRENT-bounded
echo TP2-DONE
