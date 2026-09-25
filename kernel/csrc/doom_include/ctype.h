#ifndef KONJAC_CTYPE_H
#define KONJAC_CTYPE_H

#ifdef __cplusplus
extern "C" {
#endif

int isalpha(int c);
int isdigit(int c);
int isalnum(int c);
int isspace(int c);
int isupper(int c);
int islower(int c);
int isxdigit(int c);
int ispunct(int c);
int iscntrl(int c);
int isprint(int c);
int toupper(int c);
int tolower(int c);

#ifdef __cplusplus
}
#endif

#endif
