import 'package:buzz/features/push/push_preferences.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  group('PushPreferences', () {
    test('defaults to all categories enabled', () {
      const prefs = PushPreferences();
      expect(prefs.mentions, isTrue);
      expect(prefs.dms, isTrue);
      expect(prefs.agentActivity, isTrue);
    });

    test('isEnabled reflects each category', () {
      const prefs = PushPreferences(mentions: false);
      expect(prefs.isEnabled(PushCategory.mentions), isFalse);
      expect(prefs.isEnabled(PushCategory.dms), isTrue);
      expect(prefs.isEnabled(PushCategory.agentActivity), isTrue);
    });

    test('copyWith updates only the specified field', () {
      const prefs = PushPreferences();
      final updated = prefs.copyWith(dms: false);
      expect(updated.mentions, isTrue);
      expect(updated.dms, isFalse);
      expect(updated.agentActivity, isTrue);
    });

    test('serialize and deserialize round-trips correctly', () {
      const prefs = PushPreferences(mentions: true, dms: false, agentActivity: true);
      final raw = prefs.serialize();
      expect(raw, '101');

      final restored = PushPreferences.deserialize(raw);
      expect(restored.mentions, isTrue);
      expect(restored.dms, isFalse);
      expect(restored.agentActivity, isTrue);
    });

    test('deserialize returns defaults for null or short input', () {
      expect(PushPreferences.deserialize(null), const PushPreferences());
      expect(PushPreferences.deserialize('1'), const PushPreferences());
      expect(PushPreferences.deserialize(''), const PushPreferences());
    });

    test('deserialize handles all-off state', () {
      const prefs = PushPreferences(
        mentions: false,
        dms: false,
        agentActivity: false,
      );
      final restored = PushPreferences.deserialize(prefs.serialize());
      expect(restored.mentions, isFalse);
      expect(restored.dms, isFalse);
      expect(restored.agentActivity, isFalse);
    });
  });

  group('PushCategory', () {
    test('all categories have labels', () {
      for (final cat in PushCategory.values) {
        expect(cat.label, isNotEmpty);
      }
    });

    test('all categories have descriptions', () {
      for (final cat in PushCategory.values) {
        expect(cat.description, isNotEmpty);
      }
    });
  });
}
