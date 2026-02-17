/* ctype.h shim for bare-metal SQLite */
#ifndef _CTYPE_H
#define _CTYPE_H

int isspace(int c);
int isdigit(int c);
int isalpha(int c);
int isalnum(int c);
int isupper(int c);
int islower(int c);
int toupper(int c);
int tolower(int c);
int isxdigit(int c);
int isprint(int c);

#endif /* _CTYPE_H */
