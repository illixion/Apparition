extern int ap_fixture_value(void);
int ap_fixture_call(void) { return ap_fixture_value(); }
extern int ap_entry_value(void);
int ap_entry_call(void) { return ap_entry_value(); }
