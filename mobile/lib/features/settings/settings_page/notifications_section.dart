part of '../settings_page.dart';

/// Push notification preferences and master toggle.
///
/// Shows the current push status and lets the user toggle individual
/// notification categories. Category toggles are disabled when push
/// itself is off, so the hierarchy is visually clear.
class _NotificationsSection extends ConsumerWidget {
  const _NotificationsSection();

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final pushState = ref.watch(pushProvider);
    final prefs = ref.watch(pushPreferencesProvider);
    final isActive = pushState.status == PushStatus.active;

    return AppListCard(
      label: 'Notifications',
      children: [
        AppListRow(
          icon: LucideIcons.bell,
          title: 'Push Notifications',
          subtitle: _statusSubtitle(pushState.status),
          trailing: Switch.adaptive(
            value: isActive,
            onChanged: (value) => _onMasterToggle(ref, value),
          ),
        ),
        for (final category in PushCategory.values)
          AppListRow(
            icon: _categoryIcon(category),
            title: category.label,
            subtitle: category.description,
            trailing: Switch.adaptive(
              value: prefs.isEnabled(category),
              onChanged: isActive
                  ? (v) => _onCategoryToggle(ref, category, v)
                  : null,
            ),
          ),
      ],
    );
  }

  String _statusSubtitle(PushStatus status) => switch (status) {
        PushStatus.active => 'Active',
        PushStatus.registering => 'Registering…',
        PushStatus.activating => 'Activating…',
        PushStatus.denied => 'Permission denied',
        PushStatus.error => 'Error — try again',
        PushStatus.disabled => 'Off',
        PushStatus.idle => 'Off',
      };

  IconData _categoryIcon(PushCategory category) => switch (category) {
        PushCategory.mentions => LucideIcons.atSign,
        PushCategory.dms => LucideIcons.mail,
        PushCategory.agentActivity => LucideIcons.bot,
      };

  void _onMasterToggle(WidgetRef ref, bool value) {
    if (value) {
      ref.read(pushProvider.notifier).enable();
    } else {
      ref.read(pushProvider.notifier).disable();
    }
  }

  void _onCategoryToggle(
    WidgetRef ref,
    PushCategory category,
    bool value,
  ) {
    ref.read(pushPreferencesProvider.notifier).toggle(category, value: value);
    // Re-publish the lease so the relay applies the new filters immediately.
    ref.read(pushProvider.notifier).refreshSubscriptions();
  }
}
