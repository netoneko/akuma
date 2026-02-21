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

tcc -I/src/github.com/neatvi /src/github.com/neatvi/cmd.c /src/github.com/neatvi/conf.c /src/github.com/neatvi/dir.c /src/github.com/neatvi/ex.c /src/github.com/neatvi/lbuf.c /src/github.com/neatvi/led.c /src/github.com/neatvi/mot.c /src/github.com/neatvi/reg.c /src/github.com/neatvi/regex.c /src/github.com/neatvi/ren.c /src/github.com/neatvi/rset.c /src/github.com/neatvi/rstr.c /src/github.com/neatvi/sbuf.c /src/github.com/neatvi/syn.c /src/github.com/neatvi/tag.c /src/github.com/neatvi/term.c /src/github.com/neatvi/uc.c /src/github.com/neatvi/vi.c -o /bin/vi
```

