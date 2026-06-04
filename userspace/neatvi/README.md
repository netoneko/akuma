# Neatvi editor

Compiling neatvi on the target system:

```bash
# clone the repo

scratch clone https://github.com/netoneko/neatvi.git
cd neatvi
```

```bash
# install the compiler and libc

pkg install tcc tar libc
```

```bash
# compile neatvi

tcc -I/neatvi /neatvi/cmd.c /neatvi/conf.c /neatvi/dir.c /neatvi/ex.c /neatvi/lbuf.c /neatvi/led.c /neatvi/mot.c /neatvi/reg.c /neatvi/regex.c /neatvi/ren.c /neatvi/rset.c /neatvi/rstr.c /neatvi/sbuf.c /neatvi/syn.c /neatvi/tag.c /neatvi/term.c /neatvi/uc.c /neatvi/vi.c -o /bin/vi
```

