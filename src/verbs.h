#include <infiniband/verbs.h>

/* The extended port query is inline in verbs.h. Supply the legacy export
   through the retained runtime table, as in OrbiTE's sys/port.c. */
int orbit_query_port(struct ibv_context *, uint8_t, struct ibv_port_attr *,
                     int (*)(struct ibv_context *, uint8_t, struct _compat_ibv_port_attr *));

uint32_t orbit_port_speed_bits(const struct ibv_port_attr *);
