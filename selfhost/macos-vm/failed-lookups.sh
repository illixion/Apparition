#!/bin/bash
# failed-lookups.sh: run as root INSIDE the appliance guest, a few minutes after boot or after a soak.
# Lists the mach services that processes keep failing to look up, which is what a launchd service we disabled but
# something still wants looks like. A disabled biometrickitd was hit 9,000 times in four minutes; a disabled
# bluetoothd made coreaudiod retry and leak about 40 MB a minute. Dozens in four minutes is background noise,
# thousands is a service to take off appliance-disabled-*.txt.
log show --last "${1:-4}m" --style compact --predicate 'eventMessage CONTAINS "activating connection" OR eventMessage CONTAINS "failed to do a bootstrap look-up"' 2>/dev/null |
awk '
  /activating connection/ { if (match($0, /\[0x[0-9a-f]+\]/)) { id=substr($0,RSTART,RLENGTH); if (match($0, /name=[^ ]+/)) name[id]=substr($0,RSTART+5,RLENGTH-5) } }
  /failed to do a bootstrap look-up/ { if (match($0, /\[0x[0-9a-f]+\]/)) { id=substr($0,RSTART,RLENGTH); if (id in name) count[name[id]]++ } }
  END { for (n in count) printf "%5d %s\n", count[n], n }' | sort -rn | head -25
