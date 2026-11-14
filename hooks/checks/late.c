// SPDX-License-Identifier: MIT
extern int ap_entry_value(void);
static int initial;
__attribute__((constructor)) static void load(void) { initial = ap_entry_value(); }
int ap_late_initial(void) { return initial; }
