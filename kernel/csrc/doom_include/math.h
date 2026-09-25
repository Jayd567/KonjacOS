#ifndef KONJAC_MATH_H
#define KONJAC_MATH_H

#ifdef __cplusplus
extern "C" {
#endif

double sin(double x);
double cos(double x);
double tan(double x);
double atan(double x);
double atan2(double y, double x);
double fabs(double x);
double sqrt(double x);
double pow(double base, double exp);
double floor(double x);
double ceil(double x);
double fmod(double x, double y);
double log(double x);
double exp(double x);

#ifdef __cplusplus
}
#endif

#endif
