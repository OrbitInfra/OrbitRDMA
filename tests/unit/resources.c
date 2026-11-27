/* Public-header CPU provider. No RNIC or kernel verbs device is opened. */
#define ORBITRDMA_RESOURCE_TEST
#include "native.c"
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>

struct test_cq {
    struct ibv_cq cq;
    struct ibv_wc data[256], last;
    unsigned head, count;
    int armed, fired;
};
struct test_channel {
    struct ibv_comp_channel base;
    int tx;
    struct test_cq *cq;
};
struct receive_slot {
    struct ibv_sge sge;
    uint64_t id;
};
struct test_qp {
    struct ibv_qp qp;
    int stage;
    uint32_t peer;
    struct test_qp *next_qp;
    struct receive_slot receives[128];
    unsigned nrecv;
    int deferred_receive;
};
struct test_mr {
    struct ibv_mr mr;
    uint8_t *late;
    struct test_mr *next;
};
struct test_mw {
    struct ibv_mw mw;
    struct test_mw *next;
    struct test_qp *qp;
    struct test_mr *mr;
    uint64_t address, length;
    uint32_t key, access;
    uint8_t *late;
    int valid;
};
static struct test_qp *all_qps;
static struct test_mr *all_mrs;
static struct test_mw *all_mws;
static uint32_t next_lkey = 31;
static struct test_mw *window_for(uint32_t qpn, uint32_t key) {
    for (struct test_mw *w = all_mws; w; w = w->next)
        if (w->valid && w->qp && w->qp->qp.qp_num == qpn && w->key == key) return w;
    return NULL;
}
int orbit_allows(uint32_t qpn, uint32_t key, uint64_t address, uint64_t n, int write) {
    struct test_mw *w = window_for(qpn, key);
    return w && (w->access & (write ? IBV_ACCESS_REMOTE_WRITE : IBV_ACCESS_REMOTE_READ)) &&
           n && n <= w->length && address >= w->address && address - w->address <= w->length - n;
}
static char first_fault[64], second_fault[64], option[64], history[262144];
static size_t used;
static uint32_t qpn = 0x1200;
void orbit_fault(const char *first, const char *second) {
    snprintf(first_fault, sizeof(first_fault), "%s", first);
    snprintf(second_fault, sizeof(second_fault), "%s", second);
}
void orbit_option(const char *value) { snprintf(option, sizeof(option), "%s", value); }
const char *orbit_history(void) { return history; }
static int event(const char *op) {
    int n = snprintf(history + used, sizeof(history) - used, "%s ", op);
    if (n < 0 || (size_t)n >= sizeof(history) - used)
        abort();
    used += (size_t)n;
    if (!strcmp(op, first_fault) || !strcmp(op, second_fault)) {
        errno = EIO;
        return EIO;
    }
    return 0;
}
static int is(const char *s) { return !strcmp(option, s); }
#include "payload.h"
static union ibv_gid port_gid(uint32_t port) {
    union ibv_gid gid = {.raw = {0x20, 1}};
    gid.raw[15] = (uint8_t)port;
    return gid;
}
static struct ibv_mw *alloc_mw(struct ibv_pd *pd, enum ibv_mw_type type) {
    if (event("alloc_mw") || type != IBV_MW_TYPE_2)
        return NULL;
    struct test_mw *w = calloc(1, sizeof(*w));
    if (!w)
        return NULL;
    struct ibv_mw *mw = &w->mw;
    mw->pd = pd;
    mw->context = pd->context;
    mw->type = type;
    mw->rkey = is("other_index") ? 0x5500 : 0x5400;
    for (struct test_mw *live = all_mws; live; live = live->next)
        if (live->mw.rkey >= mw->rkey) mw->rkey = live->mw.rkey + 0x100;
    w->next = all_mws;
    all_mws = w;
    return mw;
}
static int free_mw(struct ibv_mw *mw) {
    if (event("free_mw"))
        return EIO;
    struct test_mw *w = (struct test_mw *)mw;
    struct test_mw **it = &all_mws;
    while (*it && *it != w) it = &(*it)->next;
    if (!*it) return EINVAL;
    *it = w->next;
    /* Model a previously translated WRITE that still holds the target MR. */
    if (w->late) w->mr->late = w->late;
    free(w);
    return 0;
}
static int post(struct ibv_qp *base, struct ibv_send_wr *wr, struct ibv_send_wr **bad) {
    struct test_qp *q = (struct test_qp *)base;
    if (wr->opcode != IBV_WR_BIND_MW && wr->opcode != IBV_WR_LOCAL_INV)
        return data_send(q, wr, bad);
    struct test_cq *cq = (struct test_cq *)base->send_cq;
    const int bind = wr->opcode == IBV_WR_BIND_MW;
    if (q->stage != 3 || wr->next || wr->send_flags != IBV_SEND_SIGNALED)
        return EINVAL;
    if ((bind && is("reject_bind")) || (!bind && is("reject_invalidate"))) {
        *bad = wr;
        return ENOMEM;
    }
    if (bind) {
        struct ibv_mw_bind_info *info = &wr->bind_mw.bind_info;
        struct ibv_mr *mr = info->mr;
        uint64_t start = (uintptr_t)mr->addr;
        if (wr->bind_mw.mw->pd != base->pd || mr->pd != base->pd || info->addr < start ||
            info->length == 0 || info->length > mr->length ||
            info->addr - start > mr->length - info->length ||
            !(info->mw_access_flags & (IBV_ACCESS_REMOTE_READ | IBV_ACCESS_REMOTE_WRITE)) ||
            (info->mw_access_flags & ~(IBV_ACCESS_REMOTE_READ | IBV_ACCESS_REMOTE_WRITE)))
            return EINVAL;
        struct test_mw *w = (struct test_mw *)wr->bind_mw.mw;
        w->valid = 1;
        w->qp = q;
        w->mr = (struct test_mr *)mr;
        w->key = wr->bind_mw.rkey;
        w->address = info->addr;
        w->length = info->length;
        w->access = info->mw_access_flags;
        if (w->access & IBV_ACCESS_REMOTE_WRITE)
            w->late = (uint8_t *)(uintptr_t)(w->address + w->length - 1);
    } else {
        struct test_mw *w = window_for(base->qp_num, wr->invalidate_rkey);
        if (!w) return EINVAL;
        w->valid = 0;
    }
    struct ibv_wc wc = {.wr_id = wr->wr_id,
                        .qp_num = base->qp_num,
                        .status = IBV_WC_SUCCESS,
                        .opcode = bind ? IBV_WC_BIND_MW : IBV_WC_LOCAL_INV};
    enqueue(cq, wc);
    if (event(bind ? "bind" : "invalidate")) {
        *bad = NULL;
        return EIO;
    } /* ambiguous submission */
    return 0;
}
static int poll(struct ibv_cq *base, int n, struct ibv_wc *wc) {
    struct test_cq *cq = (struct test_cq *)base;
    if (event("poll"))
        return -EIO;
    if (n != 1)
        return -EINVAL;
    if (!cq->count || is("delay")) return 0;
    *wc = cq->data[cq->head++ % 256];
    --cq->count;
    if (wc->opcode != IBV_WC_BIND_MW && wc->opcode != IBV_WC_LOCAL_INV) return 1;
    if (is("foreign_id"))
        ++wc->wr_id;
    if (is("foreign_qp"))
        ++wc->qp_num;
    if (is("wrong_opcode"))
        wc->opcode = IBV_WC_SEND;
    if (is("failed_wc"))
        wc->status = IBV_WC_WR_FLUSH_ERR;
    return 1;
}
static void fill_port(uint8_t p, struct ibv_port_attr *a) {
    a->gid_tbl_len = 2;
    a->state = is("down") ? IBV_PORT_DOWN : IBV_PORT_ACTIVE;
    a->active_mtu = is("new_mtu") ? IBV_MTU_512 : IBV_MTU_1024;
    a->lid = p == 2 ? 17 : 0;
    a->link_layer = p == 2 ? IBV_LINK_LAYER_INFINIBAND : IBV_LINK_LAYER_ETHERNET;
}
static int extended_port(struct ibv_context *c, uint8_t p, struct ibv_port_attr *a, size_t len) {
    (void)c;
    if (event("port_ex") || len != sizeof(*a))
        return EINVAL;
    fill_port(p, a);
    return 0;
}
struct ibv_context *ibv_open_device(struct ibv_device *d) {
    if (event("open"))
        return NULL;
    struct ibv_context *c;
    if (is("extended_port")) {
        struct verbs_context *v = calloc(1, sizeof(*v));
        if (!v)
            return NULL;
        v->sz = sizeof(*v);
        v->query_port = extended_port;
        v->context.abi_compat = __VERBS_ABI_IS_EXTENDED;
        c = &v->context;
    } else {
        c = calloc(1, sizeof(*c));
        if (!c)
            return NULL;
    }
    c->async_fd = open("/dev/null", O_RDONLY | O_CLOEXEC);
    c->device = d;
    c->ops.alloc_mw = alloc_mw;
    c->ops.dealloc_mw = free_mw;
    c->ops.post_send = post;
    c->ops.post_recv = data_recv;
    c->ops.req_notify_cq = data_arm;
    c->ops.poll_cq = poll;
    return c;
}
int ibv_close_device(struct ibv_context *c) {
    if (event("close"))
        return EIO;
    if (!c)
        return EINVAL;
    close(c->async_fd);
    struct verbs_context *extended = verbs_get_ctx(c);
    if (extended)
        free(extended);
    else
        free(c);
    return 0;
}
int ibv_query_device(struct ibv_context *c, struct ibv_device_attr *a) {
    (void)c;
    if (event("caps"))
        return EIO;
    a->phys_port_cnt = 2;
    a->max_qp_wr = 128;
    a->max_cqe = 192;
    a->max_qp_rd_atom = 3;
    a->max_qp_init_rd_atom = 5;
    a->device_cap_flags = is("no_windows") ? 0 : IBV_DEVICE_MEM_WINDOW_TYPE_2B;
    return 0;
}
int ibv_query_port(struct ibv_context *c, uint8_t port, struct _compat_ibv_port_attr *out) {
    (void)c;
    if (event("port"))
        return EIO;
    fill_port(port, (struct ibv_port_attr *)out);
    return 0;
}
int ibv_query_gid(struct ibv_context *c, uint8_t p, int i, union ibv_gid *g) {
    (void)c;
    (void)i;
    if (event("gid"))
        return EIO;
    *g = port_gid(p);
    return 0;
}
int _ibv_query_gid_ex(struct ibv_context *c, uint32_t p, uint32_t i, struct ibv_gid_entry *e,
                      uint32_t flags, size_t n) {
    (void)c;
    if (event("gid_ex"))
        return EIO;
    if (n != sizeof(*e) || flags)
        return EINVAL;
    e->port_num = p;
    e->gid_index = i;
    e->gid = port_gid(p);
    e->gid_type = p == 2 ? IBV_GID_TYPE_IB : IBV_GID_TYPE_ROCE_V2;
    if (is("stale_gid"))
        ++e->gid_index;
    return 0;
}
struct ibv_pd *ibv_alloc_pd(struct ibv_context *ctx) {
    if (event("pd"))
        return NULL;
    struct ibv_pd *p = calloc(1, sizeof(*p));
    if (p)
        p->context = ctx;
    return p;
}
int ibv_dealloc_pd(struct ibv_pd *p) {
    if (event("free_pd"))
        return EIO;
    free(p);
    return 0;
}
struct ibv_comp_channel *ibv_create_comp_channel(struct ibv_context *c) {
    if (event("channel"))
        return NULL;
    struct test_channel *h = calloc(1, sizeof(*h));
    int fds[2];
    if (!h)
        return NULL;
    if (pipe(fds)) {
        free(h);
        return NULL;
    }
    h->base.context = c;
    h->base.fd = fds[0];
    h->tx = fds[1];
    return &h->base;
}
int ibv_destroy_comp_channel(struct ibv_comp_channel *h) {
    if (event("free_channel"))
        return EIO;
    close(h->fd);
    close(((struct test_channel *)h)->tx);
    free(h);
    return 0;
}
struct ibv_cq *ibv_create_cq(struct ibv_context *c, int n, void *v, struct ibv_comp_channel *h,
                             int i) {
    (void)v;
    if (event("cq") || (h && c != h->context) || n <= 0 || n > 192 || i)
        return NULL;
    struct test_cq *cq = calloc(1, sizeof(*cq));
    if (!cq)
        return NULL;
    cq->cq.context = c;
    cq->cq.channel = h;
    if (h)
        ((struct test_channel *)h)->cq = cq;
    cq->cq.cqe = is("small_cq") ? n - 1 : n;
    return &cq->cq;
}
int ibv_destroy_cq(struct ibv_cq *c) {
    if (event("free_cq"))
        return EIO;
    free(c);
    return 0;
}
struct ibv_mr *ibv_reg_mr(struct ibv_pd *p, void *a, size_t n, int flags) {
    if (event("mr") ||
        (flags != IBV_ACCESS_LOCAL_WRITE && flags != (IBV_ACCESS_LOCAL_WRITE | IBV_ACCESS_MW_BIND)))
        return NULL;
    struct test_mr *entry = calloc(1, sizeof(*entry));
    if (!entry)
        return NULL;
    entry->next = all_mrs;
    all_mrs = entry;
    struct ibv_mr *m = &entry->mr;
    m->lkey = ++next_lkey;
    m->pd = p;
    m->context = p->context;
    m->addr = a;
    m->length = n;
    m->rkey = 0xdead;
    return m;
}
int ibv_dereg_mr(struct ibv_mr *m) {
    if (event("free_mr"))
        return EIO;
    struct test_mr **it = &all_mrs;
    while (*it && &(*it)->mr != m)
        it = &(*it)->next;
    if (!*it)
        return EINVAL;
    if ((*it)->late) *(*it)->late = 0xa5;
    *it = (*it)->next;
    free(m);
    return 0;
}
struct ibv_qp *ibv_create_qp(struct ibv_pd *p, struct ibv_qp_init_attr *a) {
    if (event("qp") || a->qp_type != IBV_QPT_RC || (is("shared_cqs") ? a->send_cq == a->recv_cq : a->send_cq != a->recv_cq) ||
        a->send_cq->context != p->context || a->recv_cq->context != p->context || a->cap.max_send_sge != 1 || a->cap.max_recv_sge != 1 ||
        a->sq_sig_all != 0)
        return NULL;
    struct test_qp *q = calloc(1, sizeof(*q));
    if (!q)
        return NULL;
    q->qp.context = p->context;
    q->qp.pd = p;
    q->qp.send_cq = a->send_cq;
    q->qp.recv_cq = a->recv_cq;
    q->qp.qp_num = is("reuse_qpn") ? 0x1240 : ++qpn;
    q->next_qp = all_qps;
    all_qps = q;
    if (is("undersized"))
        a->cap.max_recv_wr = 0;
    return &q->qp;
}
static struct ibv_qp_attr rtr, rts;
unsigned orbit_attr(int n) {
    switch (n) {
    case 0:
        return rtr.dest_qp_num;
    case 1:
        return rtr.rq_psn;
    case 2:
        return rts.sq_psn;
    case 3:
        return rtr.path_mtu;
    case 4:
        return rtr.max_dest_rd_atomic;
    case 5:
        return rts.max_rd_atomic;
    case 6:
        return rtr.ah_attr.grh.sgid_index;
    case 8:
        return rtr.ah_attr.sl;
    case 9:
        return rtr.ah_attr.grh.traffic_class;
    case 10:
        return rtr.ah_attr.grh.hop_limit;
    default:
        return rtr.ah_attr.dlid;
    }
}
int ibv_modify_qp(struct ibv_qp *base, struct ibv_qp_attr *a, int mask) {
    struct test_qp *q = (struct test_qp *)base;
    switch (a->qp_state) {
    case IBV_QPS_INIT:
        if (event("init"))
            return EIO;
        if (q->stage ||
            mask != (IBV_QP_STATE | IBV_QP_PKEY_INDEX | IBV_QP_PORT | IBV_QP_ACCESS_FLAGS) ||
            a->qp_access_flags != (is("shared_cqs") ? IBV_ACCESS_REMOTE_READ : (IBV_ACCESS_REMOTE_READ | IBV_ACCESS_REMOTE_WRITE)) ||
            !a->port_num || a->pkey_index)
            return EINVAL;
        break;
    case IBV_QPS_RTR:
        if (event("rtr"))
            return EIO;
        if (q->stage != 1 ||
            mask != (IBV_QP_STATE | IBV_QP_AV | IBV_QP_PATH_MTU | IBV_QP_DEST_QPN | IBV_QP_RQ_PSN |
                     IBV_QP_MAX_DEST_RD_ATOMIC | IBV_QP_MIN_RNR_TIMER) ||
            !a->ah_attr.port_num)
            return EINVAL;
        if (is("lid_only") ? (a->ah_attr.is_global || !a->ah_attr.dlid)
                           : (!a->ah_attr.is_global || !a->ah_attr.grh.hop_limit))
            return EINVAL;
        rtr = *a;
        q->peer = a->dest_qp_num;
        break;
    case IBV_QPS_RTS:
        if (event("rts"))
            return EIO;
        if (q->stage != 2 || mask != (IBV_QP_STATE | IBV_QP_TIMEOUT | IBV_QP_RETRY_CNT |
                                      IBV_QP_RNR_RETRY | IBV_QP_SQ_PSN | IBV_QP_MAX_QP_RD_ATOMIC))
            return EINVAL;
        rts = *a;
        break;
    default:
        return EINVAL;
    }
    ++q->stage;
    return 0;
}
int ibv_destroy_qp(struct ibv_qp *base) {
    if (event("free_qp"))
        return EIO;
    struct test_qp *q = (struct test_qp *)base;
    /* An earlier accepted remote WRITE becomes visible only at physical drain. */
    if (q->deferred_receive) {
        for (unsigned i = 0; i < q->nrecv; ++i) {
            struct ibv_sge *s = &q->receives[i].sge;
            if (!valid_sge(q, s))
                abort();
            ((uint8_t *)(uintptr_t)s->addr)[s->length - 1] = 0xb6;
        }
    }
    struct test_qp **it = &all_qps;
    while (*it && *it != q)
        it = &(*it)->next_qp;
    if (!*it)
        return EINVAL;
    *it = q->next_qp;
    for (struct test_mw *w = all_mws; w; w = w->next) {
        if (w->qp != q) continue;
        if (w->late) *w->late = 0xa5;
        w->late = NULL;
        w->valid = 0;
        w->qp = NULL;
    }
    free(q);
    return 0;
}
int ibv_query_qp(struct ibv_qp *q, struct ibv_qp_attr *a, int m, struct ibv_qp_init_attr *i) {
    (void)q;
    (void)a;
    (void)m;
    (void)i;
    return ENOSYS;
}
int ibv_get_cq_event(struct ibv_comp_channel *h, struct ibv_cq **q, void **v) {
    if (event("get_event")) {
        errno = EIO;
        return -1;
    }
    struct test_channel *ch = (struct test_channel *)h;
    char token;
    if (read(h->fd, &token, 1) != 1)
        return -1;
    *q = &ch->cq->cq;
    *v = NULL;
    if (is("on_event") || (is("spurious") && ch->cq->fired == 2))
        enqueue(ch->cq, ch->cq->last);
    return 0;
}
void ibv_ack_cq_events(struct ibv_cq *q, unsigned n) {
    (void)q;
    if (n != 1)
        abort();
    event("ack");
}
int ibv_get_async_event(struct ibv_context *c, struct ibv_async_event *e) {
    (void)c;
    (void)e;
    return ENOSYS;
}
void ibv_ack_async_event(struct ibv_async_event *e) { (void)e; }
