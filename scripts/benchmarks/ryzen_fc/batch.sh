#!/bin/bash
# batch.sh FIRST LAST [jobs]: sequential fresh-boot builds
cd /home/netoneko/akuma-selfhost
for n in $(seq $1 $2); do ./jrun.sh $n ${3:-4}; done
echo done > runs/batch-$1-$2.done
