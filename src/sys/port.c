/* Adapted from OrbiTE's extended/legacy port-query bridge. */
#include "../verbs.h"
#include <errno.h>

int orbit_query_port(struct ibv_context *ctx, uint8_t port, struct ibv_port_attr *out,
                     int (*legacy)(struct ibv_context *, uint8_t, struct _compat_ibv_port_attr *))
{
    struct verbs_context *vctx = verbs_get_ctx_op(ctx, query_port);
    if (vctx)
        return vctx->query_port(ctx, port, out, sizeof(*out));
    return legacy ? legacy(ctx, port, (struct _compat_ibv_port_attr *)out) : ENOSYS;
}

uint32_t orbit_port_speed_bits(const struct ibv_port_attr *attr) {
#ifdef ORBIT_HAVE_ACTIVE_SPEED_EX
    if (attr->active_speed_ex != 0)
        return attr->active_speed_ex;
#endif
    return attr->active_speed;
}


#ifdef ORBIT_MLX5
#include <errno.h>
#if __has_include(<infiniband/mlx5dv.h>)
#include <infiniband/mlx5dv.h>
int orbit_mlx5_pdn(int (*init)(struct mlx5dv_obj *, uint64_t), void *pd, uint32_t *pdn) {
    struct mlx5dv_pd result = {0};
    struct mlx5dv_obj object = {.pd = {.in = pd, .out = &result}};
    int code = init(&object, MLX5DV_OBJ_PD);
    if (!code) *pdn = result.pdn;
    return code;
}
#else
int orbit_mlx5_pdn(int (*init)(void *, uint64_t), void *pd, uint32_t *pdn) {
    (void)init; (void)pd; (void)pdn;
    return EOPNOTSUPP;
}
#endif
#endif
