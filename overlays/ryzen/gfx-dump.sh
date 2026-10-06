#!/bin/sh
# What Linux knows about ryzen's graphics, onto p3 at /root/gfx/ (Akuma reads it
# there) — for making rio's software wgpu path faster: the GPU and its PCI
# resources, the panel's mode and timings, the CPU's vector features (a software
# rasteriser lives or dies on them), memory, the framebuffer Linux gets and what
# a Vulkan/GL stack reports. Run in Pop, as root.
#
#   sh overlays/ryzen/gfx-dump.sh
#
# Serial numbers (panel EDID, board) are dropped.
set -u
P=/mnt/p3
mountpoint -q $P || { mkdir -p $P; mount /dev/nvme0n1p3 $P; }
D=$P/root/gfx
mkdir -p $D
{
echo "## lscpu"; lscpu | grep -v -i serial
echo; echo "## memory"; grep -E 'MemTotal|HugePages_Total|Hugepagesize' /proc/meminfo
echo; echo "## dmi"; cat /sys/class/dmi/id/product_name /sys/class/dmi/id/bios_version 2>/dev/null
echo; echo "## gpu (lspci)"; lspci -nnvv -d ::0300 2>/dev/null; lspci -nnvv -d ::0380 2>/dev/null
echo; echo "## drm"; for c in /sys/class/drm/card*; do echo "$c"; done
for c in /sys/class/drm/card*-*; do
  echo "--- $c: status=$(cat $c/status 2>/dev/null) enabled=$(cat $c/enabled 2>/dev/null)"
  echo "modes:"; head -8 $c/modes 2>/dev/null
done
echo; echo "## amdgpu"
for n in mem_info_vram_total mem_info_vis_vram_total mem_info_gtt_total gpu_busy_percent pp_dpm_sclk pp_dpm_mclk power_dpm_force_performance_level; do
  for f in /sys/class/drm/card*/device/$n; do [ -r $f ] && echo "$n: $(tr '\n' ' ' < $f)"; done
done
echo; echo "## framebuffer"
for n in name virtual_size bits_per_pixel stride; do for f in /sys/class/graphics/fb*/$n; do [ -r $f ] && echo "$f: $(cat $f)"; done; done; command -v fbset >/dev/null && fbset -i 2>/dev/null
echo; echo "## efi gop (boot)"; dmesg 2>/dev/null | grep -i -E 'efifb|simplefb|fb0|framebuffer' | head -10
echo; echo "## vulkan"; command -v vulkaninfo >/dev/null && vulkaninfo --summary 2>/dev/null | grep -v -i uuid | head -40 || echo "vulkaninfo absent"
echo; echo "## gl"; command -v glxinfo >/dev/null && glxinfo -B 2>/dev/null | head -30 || echo "glxinfo absent"
echo; echo "## modetest (connectors, modes with refresh, planes)"; command -v modetest >/dev/null && modetest -M amdgpu -c 2>/dev/null | grep -v -i "serial" | head -50
echo; echo "## display scale/modes (xrandr/drm_info)"; command -v xrandr >/dev/null && xrandr --current 2>/dev/null | head -12; command -v drm_info >/dev/null && drm_info 2>/dev/null | head -60
} > $D/gfx.txt 2>&1
# The panel's EDID, decoded, without serials.
for e in /sys/class/drm/card*-eDP-*/edid; do
  [ -s $e ] || continue
  if command -v edid-decode >/dev/null; then edid-decode $e 2>/dev/null | grep -v -i serial > $D/edid-edp.txt; else echo "edid-decode absent" > $D/edid-edp.txt; fi
done
sync
echo "gfx dump -> p3:/root/gfx ($(wc -l < $D/gfx.txt) lines)"
