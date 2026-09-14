"""Render a corpus of resource trees with the reference's own classes."""
import json, sys
sys.path.insert(0, 'oracle')
from awscli.customizations.ecs.expressgateway.managedresource import ManagedResource
from awscli.customizations.ecs.expressgateway.managedresourcegroup import ManagedResourceGroup as G

def R(**kw):
    return ManagedResource(
        kw.get('type'), kw.get('id'), status=kw.get('status'),
        updated_at=kw.get('updated_at'), reason=kw.get('reason'),
        additional_info=kw.get('info'))

def spec_leaf(**kw):
    return {'kind': 'leaf', **kw}

def spec_group(**kw):
    return {'kind': 'group', **kw}

def build(spec):
    if spec['kind'] == 'leaf':
        return R(**{k: v for k, v in spec.items() if k != 'kind'})
    return G(resource_type=spec.get('type'), identifier=spec.get('id'),
             status=spec.get('status'), reason=spec.get('reason'),
             resources=[build(c) for c in spec.get('children', [])])

TREES = {
  "leaf_full": spec_leaf(type='LoadBalancer', id='arn:lb/1', status='PROVISIONING',
                         updated_at='2026-09-14T10:00:00Z', reason='creating', info='note'),
  "leaf_active": spec_leaf(type='TargetGroup', id='arn:tg/1', status='ACTIVE'),
  "leaf_failed": spec_leaf(type='Rule', id='arn:rule/1', status='FAILED', reason='boom'),
  "leaf_deleted": spec_leaf(type='Listener', id='arn:l/1', status='DELETED'),
  "leaf_rollback_failed": spec_leaf(type='X', id='x', status='ROLLBACK_FAILED'),
  "leaf_stopped": spec_leaf(type='X', id='x', status='STOPPED'),
  "leaf_rollback_ok": spec_leaf(type='X', id='x', status='ROLLBACK_SUCCESSFUL'),
  "leaf_successful": spec_leaf(type='X', id='x', status='SUCCESSFUL'),
  "leaf_no_status": spec_leaf(type='Cluster', id='arn:cluster/1'),
  "leaf_no_id": spec_leaf(type='Cluster'),
  "leaf_nothing": spec_leaf(),
  "group_empty": spec_group(type='LogGroups'),
  "group_untyped_empty": spec_group(),
  "group_plain": spec_group(type='LogGroups', children=[
      spec_leaf(type='LogGroup', id='lg-1', status='ACTIVE'),
      spec_leaf(type='LogGroup', id='lg-2', status='PROVISIONING')]),
  "group_with_id_no_status": spec_group(type='IngressPath', id='https://x', children=[
      spec_leaf(type='LoadBalancer', id='lb', status='ACTIVE')]),
  "group_with_id_and_status": spec_group(type='Deployment', id='arn:dep/1',
      status='IN_PROGRESS', reason='rolling', children=[
      spec_leaf(type='TaskDefinition', id='td:1')]),
  "group_status_no_id": spec_group(type='Deployment', status='SUCCESSFUL', reason='ok',
      children=[spec_leaf(type='A', id='a')]),
  "group_nested": spec_group(children=[
      spec_leaf(type='Cluster', id='c1'),
      spec_group(type='IngressPaths', children=[
          spec_group(type='IngressPath', id='https://a', children=[
              spec_leaf(type='LoadBalancer', id='lb-a', status='PROVISIONING',
                        updated_at='2026-01-02T03:04:05Z'),
              spec_leaf(type='TargetGroup', id='tg-a', status='ACTIVE')]),
          spec_group(type='IngressPath', id='https://b')]),
      spec_group(type='AutoScalingConfiguration', children=[])]),
  "group_duplicate_keys": spec_group(type='Dupes', children=[
      spec_leaf(type='A', id='1', status='ACTIVE'),
      spec_leaf(type='A', id='1', status='FAILED'),
      spec_leaf(type='B', id='2')]),
}

cases = []
for name, spec in TREES.items():
    node = build(spec)
    case = {
        'name': name,
        'tree': spec,
        'stream_plain': node.get_stream_string('TS', use_color=False),
        'stream_color': node.get_stream_string('TS', use_color=True),
        'is_terminal': node.is_terminal(),
    }
    try:
        case['status_plain'] = node.get_status_string('*', use_color=False)
        case['status_color'] = node.get_status_string('*', use_color=True)
        case['status_depth2'] = node.get_status_string('*', depth=2, use_color=False)
    except TypeError as e:
        # The reference crashes rendering a resource with no type in the nested view.
        case['status_error'] = str(e)
    cases.append(case)

# combine and compare_sets pairs
PAIRS = {
  "combine_disjoint": (
      spec_group(children=[spec_leaf(type='A', id='1', status='ACTIVE')]),
      spec_group(children=[spec_leaf(type='B', id='2', status='ACTIVE')])),
  "combine_same_key_newer_wins": (
      spec_group(children=[spec_leaf(type='A', id='1', status='OLD', updated_at='2026-01-01T00:00:00Z')]),
      spec_group(children=[spec_leaf(type='A', id='1', status='NEW', updated_at='2026-02-01T00:00:00Z')])),
  "combine_drops_unidentified_of_same_type": (
      spec_group(children=[spec_leaf(type='A'), spec_leaf(type='B', id='b')]),
      spec_group(children=[spec_leaf(type='A', id='a')])),
  "combine_regroups_by_type": (
      spec_group(children=[spec_leaf(type='A', id='1'), spec_leaf(type='B', id='1')]),
      spec_group(children=[spec_leaf(type='A', id='2'), spec_leaf(type='B', id='2')])),
  "combine_nested_groups": (
      spec_group(children=[spec_group(type='G', children=[spec_leaf(type='A', id='1')])]),
      spec_group(children=[spec_group(type='G', children=[spec_leaf(type='A', id='2')])])),
}
combines = []
for name, (a, b) in PAIRS.items():
    combined = build(a).combine(build(b))
    combines.append({'name': name, 'left': a, 'right': b,
                     'status_plain': combined.get_status_string('*', use_color=False)})

COMPARES = {
  "compare_added_and_removed": (
      spec_group(children=[spec_leaf(type='A', id='1'), spec_leaf(type='B', id='2')]),
      spec_group(children=[spec_leaf(type='A', id='1'), spec_leaf(type='C', id='3')])),
  "compare_unidentified_suppresses_other": (
      spec_group(children=[spec_leaf(type='A')]),
      spec_group(children=[spec_leaf(type='A', id='1'), spec_leaf(type='B', id='2')])),
  "compare_nested": (
      spec_group(children=[spec_group(type='G', children=[
          spec_leaf(type='A', id='1'), spec_leaf(type='A', id='2')])]),
      spec_group(children=[spec_group(type='G', children=[
          spec_leaf(type='A', id='2'), spec_leaf(type='A', id='3')])])),
  "compare_identical": (
      spec_group(children=[spec_leaf(type='A', id='1')]),
      spec_group(children=[spec_leaf(type='A', id='1')])),
}
compares = []
for name, (a, b) in COMPARES.items():
    left, right = build(a).compare_resource_sets(build(b))
    compares.append({'name': name, 'left': a, 'right': b,
                     'unique_left': left.get_status_string('*', use_color=False),
                     'unique_right': right.get_status_string('*', use_color=False)})

# The nested view renders local time, so the corpus is only comparable line for line
# on a machine at the same offset. Record it so the test can say so.
from datetime import datetime
offset = int(datetime.now().astimezone().utcoffset().total_seconds())
json.dump({'tz_offset_seconds': offset, 'render': cases, 'combine': combines,
           'compare': compares}, sys.stdout, indent=1)
