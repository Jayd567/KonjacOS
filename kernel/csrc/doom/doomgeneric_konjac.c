// KonjacOS platform driver for doomgeneric: implements the six functions
// doomgeneric.h asks every port to provide (DG_Init/DG_DrawFrame/
// DG_SleepMs/DG_GetTicksMs/DG_GetKey/DG_SetWindowTitle), on top of the
// glue functions doom_driver.rs exports (konjac_doom_blit/
// konjac_doom_ticks_ms/konjac_doom_sleep_ms/konjac_doom_get_key).
//
// This file plays the same role doomgeneric_linuxvt.c/doomgeneric_sdl.c
// play in upstream doomgeneric -- it's deliberately kept out of the
// "core" file list in build.rs (which mirrors Makefile.linuxvt's SRC_DOOM
// minus doomgeneric_linuxvt.c) since it's KonjacOS-specific, not portable
// engine code.

#include <stdio.h>

#include "doomgeneric.h"

extern void konjac_doom_blit(const uint32_t *buf, uint32_t width, uint32_t height);
extern uint32_t konjac_doom_ticks_ms(void);
extern void konjac_doom_sleep_ms(uint32_t ms);
extern int konjac_doom_get_key(int *pressed, unsigned char *key);

void DG_Init(void)
{
    printf("DG_Init: KonjacOS doomgeneric driver ready (%dx%d).\n",
           DOOMGENERIC_RESX, DOOMGENERIC_RESY);
}

void DG_DrawFrame(void)
{
    konjac_doom_blit((const uint32_t *)DG_ScreenBuffer, DOOMGENERIC_RESX, DOOMGENERIC_RESY);
}

void DG_SleepMs(uint32_t ms)
{
    konjac_doom_sleep_ms(ms);
}

uint32_t DG_GetTicksMs(void)
{
    return konjac_doom_ticks_ms();
}

int DG_GetKey(int *pressed, unsigned char *key)
{
    return konjac_doom_get_key(pressed, key);
}

void DG_SetWindowTitle(const char *title)
{
    (void)title;
    // No window system to set a title on -- this is a no-op, same as
    // most non-windowed doomgeneric ports.
}
