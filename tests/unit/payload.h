/* Header-included controls for the test-only verbs DSO. Each subprocess owns its state. */
static int stop_at = -1, bad_style;
void orbit_post_result(int accepted, int style) {
    stop_at = accepted;
    bad_style = style;
}
static struct test_qp *find_qp(uint32_t n) {
    for (struct test_qp *q = all_qps; q; q = q->next_qp)
        if (q->qp.qp_num == n)
            return q;
    return NULL;
}
static int valid_sge(struct test_qp *q, const struct ibv_sge *s) {
    if (!s || !s->length)
        return 0;
    for (struct test_mr *e = all_mrs; e; e = e->next) {
        struct ibv_mr *m = &e->mr;
        uint64_t a = (uintptr_t)m->addr;
        if (m->pd == q->qp.pd && m->lkey == s->lkey && s->addr >= a && s->length <= m->length &&
            s->addr - a <= m->length - s->length)
            return 1;
    }
    return 0;
}
static void wake(struct test_cq *cq) {
    struct test_channel *ch = (struct test_channel *)cq->cq.channel;
    if (write(ch->tx, "w", 1) != 1)
        abort();
    cq->armed = 0;
}
static void enqueue(struct test_cq *cq, struct ibv_wc wc) {
    if (cq->count >= 256)
        abort();
    cq->data[(cq->head + cq->count++) % 256] = wc;
    if (cq->armed)
        wake(cq);
}
int orbit_cqe(uint32_t queue, uint64_t id, uint32_t reported_qp, unsigned opcode, unsigned bytes,
              unsigned status, unsigned flags, uint32_t key) {
    struct test_qp *q = find_qp(queue);
    if (!q)
        return ENOENT;
    for (unsigned i = 0; i < q->nrecv && reported_qp == queue; ++i) {
        struct receive_slot *slot = &q->receives[i];
        if (slot->id == id && (status || (opcode == IBV_WC_RECV && bytes <= slot->sge.length))) {
            memmove(slot, slot + 1, (--q->nrecv - i) * sizeof(*slot));
            break;
        }
    }
    struct ibv_wc wc = {.wr_id = id,
                        .qp_num = reported_qp,
                        .opcode = opcode,
                        .byte_len = bytes,
                        .status = status,
                        .wc_flags = flags,
                        .invalidated_rkey = key,
                        .vendor_err = 0x21};
    enqueue((struct test_cq *)q->qp.send_cq, wc);
    return 0;
}
static int data_arm(struct ibv_cq *base, int solicited) {
    struct test_cq *cq = (struct test_cq *)base;
    if (event("arm") || solicited)
        return EIO;
    cq->armed = 1;
    if (is("arm_race"))
        enqueue(cq, cq->last);
    ++cq->fired;
    if (is("on_event") || is("spurious"))
        wake(cq);
    return 0;
}
static int data_recv(struct ibv_qp *base, struct ibv_recv_wr *wr, struct ibv_recv_wr **bad) {
    static struct ibv_recv_wr foreign;
    struct test_qp *q = (struct test_qp *)base;
    struct test_cq *cq = (struct test_cq *)base->recv_cq;
    int index = 0;
    event("post_recv");
    for (; wr; wr = wr->next, ++index) {
        if (index == stop_at) {
            *bad = bad_style == 1 ? NULL : bad_style == 2 ? &foreign : wr;
            return bad_style == 3 ? 0 : ENOMEM;
        }
        if (q->stage != 3 || wr->num_sge < 1 || wr->num_sge > 8 || !valid_sge(q, wr->sg_list) || q->nrecv >= 128) {
            *bad = wr;
            return EINVAL;
        }
        for (int i = 0; i < wr->num_sge; ++i) if (!valid_sge(q, &wr->sg_list[i])) {
            *bad = wr; return EINVAL;
        }
        /* Multi-SGE byte movement is tested with RXE. This CPU mode injects CQEs. */
        if (wr->num_sge > 1 && !is("manual")) { *bad = wr; return EOPNOTSUPP; }
        event("rq_wr");
        cq->last = (struct ibv_wc){.wr_id = wr->wr_id,
                                   .qp_num = base->qp_num,
                                   .opcode = IBV_WC_RECV,
                                   .status = IBV_WC_SUCCESS,
                                   .byte_len = wr->sg_list->length};
        q->receives[q->nrecv++] = (struct receive_slot){*wr->sg_list, wr->wr_id};
        if (is("late_dma"))
            q->deferred_receive = 1;
    }
    return 0;
}
static enum ibv_wc_status deliver_send(struct test_qp *sender, const struct ibv_sge *s) {
    struct test_qp *peer = find_qp(sender->peer);
    if (!peer || peer->peer != sender->qp.qp_num || !peer->nrecv)
        return IBV_WC_RNR_RETRY_EXC_ERR;
    struct receive_slot slot = peer->receives[0];
    memmove(peer->receives, peer->receives + 1, --peer->nrecv * sizeof(slot));
    struct ibv_wc wc = {
        .wr_id = slot.id, .qp_num = peer->qp.qp_num, .opcode = IBV_WC_RECV, .byte_len = s->length};
    if (s->length > slot.sge.length || !valid_sge(peer, &slot.sge))
        wc.status = IBV_WC_LOC_LEN_ERR;
    else
        memcpy((void *)(uintptr_t)slot.sge.addr, (const void *)(uintptr_t)s->addr, s->length);
    enqueue((struct test_cq *)peer->qp.recv_cq, wc);
    return wc.status;
}
static int data_send(struct test_qp *q, struct ibv_send_wr *wr, struct ibv_send_wr **bad) {
    static struct ibv_send_wr foreign;
    struct test_cq *cq = (struct test_cq *)q->qp.send_cq;
    int index = 0;
    event("post_send");
    for (; wr; wr = wr->next, ++index) {
        if (index == stop_at) {
            *bad = bad_style == 1 ? NULL : bad_style == 2 ? &foreign : wr;
            return bad_style == 3 ? 0 : ENOMEM;
        }
        struct ibv_sge *s = wr->sg_list;
        if (q->stage != 3 || wr->num_sge < 1 || wr->num_sge > 8 || !valid_sge(q, s) ||
            (wr->send_flags & ~(IBV_SEND_SIGNALED | IBV_SEND_FENCE))) {
            *bad = wr;
            return EINVAL;
        }
        for (int i = 0; i < wr->num_sge; ++i) if (!valid_sge(q, &wr->sg_list[i])) {
            *bad = wr; return EINVAL;
        }
        if (wr->num_sge > 1 && !is("manual")) { *bad = wr; return EOPNOTSUPP; }
        enum ibv_wc_opcode opcode;
        switch (wr->opcode) {
        case IBV_WR_ATOMIC_FETCH_AND_ADD:
            if (!is("manual")) { *bad = wr; return EOPNOTSUPP; }
            opcode = IBV_WC_FETCH_ADD; break;
        case IBV_WR_ATOMIC_CMP_AND_SWP:
            if (!is("manual")) { *bad = wr; return EOPNOTSUPP; }
            opcode = IBV_WC_COMP_SWAP; break;
        case IBV_WR_SEND_WITH_IMM:
            if (!is("manual")) { *bad = wr; return EOPNOTSUPP; }
            /* fall through */
        case IBV_WR_SEND:
            opcode = IBV_WC_SEND;
            break;
        case IBV_WR_RDMA_READ:
            opcode = IBV_WC_RDMA_READ;
            break;
        case IBV_WR_RDMA_WRITE_WITH_IMM:
            if (!is("manual")) { *bad = wr; return EOPNOTSUPP; }
            /* fall through */
        case IBV_WR_RDMA_WRITE:
            opcode = IBV_WC_RDMA_WRITE;
            break;
        default:
            *bad = wr;
            return EINVAL;
        }
        event("sq_wr");
        if (wr->send_flags & IBV_SEND_SIGNALED)
            event("signaled");
        if (wr->send_flags & IBV_SEND_FENCE)
            event("fence");
        struct ibv_wc wc = {
            .wr_id = wr->wr_id, .qp_num = q->qp.qp_num, .opcode = opcode, .byte_len = s->length};
        cq->last = wc;
        if (is("manual"))
            continue;
        if (wr->opcode == IBV_WR_SEND)
            wc.status = deliver_send(q, s);
        else {
            uint64_t address = wr->wr.rdma.remote_addr;
            int writing = wr->opcode == IBV_WR_RDMA_WRITE;
            struct test_mw *window = window_for(q->peer, wr->wr.rdma.rkey);
            if (!orbit_allows(q->peer, wr->wr.rdma.rkey, address, s->length, writing) ||
                window->qp->peer != q->qp.qp_num)
                wc.status = IBV_WC_REM_ACCESS_ERR;
            else if (writing) {
                memcpy((void *)(uintptr_t)address, (const void *)(uintptr_t)s->addr, s->length);
                window->late = NULL;
            } else
                memcpy((void *)(uintptr_t)s->addr, (const void *)(uintptr_t)address, s->length);
        }
        if (wc.status || (wr->send_flags & IBV_SEND_SIGNALED))
            enqueue(cq, wc);
    }
    return 0;
}
