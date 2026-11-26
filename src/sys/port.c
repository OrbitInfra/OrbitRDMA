/* Adapted from OrbiTE's extended/legacy port-query bridge. */
#include "../verbs.h"

int orbit_query_port(struct ibv_context *ctx, uint8_t port, struct ibv_port_attr *out,
                     int (*legacy)(struct ibv_context *, uint8_t, struct _compat_ibv_port_attr *))
{
    struct verbs_context *vctx = verbs_get_ctx_op(ctx, query_port);
    if (vctx)
        return vctx->query_port(ctx, port, out, sizeof(*out));
    return legacy(ctx, port, (struct _compat_ibv_port_attr *)out);
}
