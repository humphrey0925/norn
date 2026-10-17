#include <nrfx.h>
#include <nrfx_uarte.h>
#include <nrfx_timer.h>
#include <nrfx_gpiote.h>

static nrfx_timer_t timer = NRFX_TIMER_INSTANCE(NRF_TIMER0);

static void on_timer(nrf_timer_event_t event, void * context)
{
    (void)event;
    (void)context;
}

int main(void)
{
    nrfx_timer_config_t config = NRFX_TIMER_DEFAULT_CONFIG(NRFX_MHZ_TO_HZ(1));
    nrfx_timer_init(&timer, &config, on_timer);
    nrfx_timer_enable(&timer);
    return 0;
}
