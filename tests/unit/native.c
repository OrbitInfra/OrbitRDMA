/* Adapted from OrbiTE's CPU export fixture for OrbitRDMA device-list lifetime tests. */
#include <infiniband/verbs.h>
#include <errno.h>
#undef ibv_query_port
#undef ibv_reg_mr

static struct ibv_device devices[2];
static struct ibv_device *entries[] = { &devices[0], &devices[1], NULL };
static struct ibv_device *empty[] = { NULL };
static int mode, frees;

void orbit_fixture_mode(int value) { mode = value; }
int orbit_fixture_frees(void) { return frees; }

struct ibv_device **ibv_get_device_list(int *count)
{
    if (mode == 1) { *count = 0; return empty; }
    if (mode == 2) { errno = EACCES; return NULL; }
    *count = 2;
    return entries;
}

void ibv_free_device_list(struct ibv_device **list)
{
    if (list == entries || list == empty) ++frees;
}

const char *ibv_get_device_name(struct ibv_device *d)
{
    if (d == &devices[0]) return "cpu-first";
    return mode == 3 ? NULL : "cpu-\xff";
}
#ifndef ORBITRDMA_RESOURCE_TEST
struct ibv_context *ibv_open_device(struct ibv_device *d) { (void)d; return NULL; }
int ibv_close_device(struct ibv_context *c) { (void)c; return EIO; }
int ibv_query_device(struct ibv_context *c, struct ibv_device_attr *a) { (void)c; (void)a; return EIO; }
int ibv_query_port(struct ibv_context *c, uint8_t p, struct _compat_ibv_port_attr *a) { (void)c; (void)p; (void)a; return EIO; }
int ibv_query_gid(struct ibv_context *c, uint8_t p, int i, union ibv_gid *g) { (void)c; (void)p; (void)i; (void)g; return EIO; }
int _ibv_query_gid_ex(struct ibv_context *c, uint32_t p, uint32_t i, struct ibv_gid_entry *e, uint32_t f, size_t n) { (void)c; (void)p; (void)i; (void)e; (void)f; (void)n; return EIO; }
struct ibv_pd *ibv_alloc_pd(struct ibv_context *c) { (void)c; return NULL; }
int ibv_dealloc_pd(struct ibv_pd *p) { (void)p; return EIO; }
struct ibv_mr *ibv_reg_mr(struct ibv_pd *p, void *a, size_t n, int f) { (void)p; (void)a; (void)n; (void)f; return NULL; }
int ibv_dereg_mr(struct ibv_mr *m) { (void)m; return EIO; }
struct ibv_cq *ibv_create_cq(struct ibv_context *c, int n, void *v, struct ibv_comp_channel *h, int i) { (void)c; (void)n; (void)v; (void)h; (void)i; return NULL; }
int ibv_destroy_cq(struct ibv_cq *c) { (void)c; return EIO; }
struct ibv_comp_channel *ibv_create_comp_channel(struct ibv_context *c) { (void)c; return NULL; }
int ibv_destroy_comp_channel(struct ibv_comp_channel *c) { (void)c; return EIO; }
int ibv_get_cq_event(struct ibv_comp_channel *h, struct ibv_cq **q, void **v) { (void)h; (void)q; (void)v; return EIO; }
void ibv_ack_cq_events(struct ibv_cq *q, unsigned int n) { (void)q; (void)n; }
struct ibv_qp *ibv_create_qp(struct ibv_pd *p, struct ibv_qp_init_attr *a) { (void)p; (void)a; return NULL; }
int ibv_modify_qp(struct ibv_qp *q, struct ibv_qp_attr *a, int m) { (void)q; (void)a; (void)m; return EIO; }
int ibv_query_qp(struct ibv_qp *q, struct ibv_qp_attr *a, int m, struct ibv_qp_init_attr *i) { (void)q; (void)a; (void)m; (void)i; return EIO; }
int ibv_destroy_qp(struct ibv_qp *q) { (void)q; return EIO; }
int ibv_get_async_event(struct ibv_context *c, struct ibv_async_event *e) { (void)c; (void)e; return EIO; }
void ibv_ack_async_event(struct ibv_async_event *e) { (void)e; }
#endif

#ifdef ORBITRDMA_OPTIONAL_VERBS
int ibv_query_port_speed(struct ibv_context *c, uint32_t p, uint64_t *speed) { (void)c; (void)p; *speed = 2000; return 0; }
#endif
